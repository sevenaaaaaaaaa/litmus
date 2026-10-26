//! 知识场（Field）：整库的语义平面布局（docs/spec/field.md）。
//!
//! 全部是**派生层**（宪法 §1）：输入是库的正文语料，输出是坐标 / 区域 / 关联线，
//! 随时可以删掉 `.litmus/local/field.json` 重建，绝不承载用户内容。
//!
//! - 向量：次线性 TF × IDF，标题加权，L2 归一，每篇只留权重最高的 [`TERMS_PER_DOC`] 个词；
//! - 近邻：倒排表求稀疏余弦，过常见词（df 过高）不进候选生成；
//! - 布局：首次 = 哈希随机投影（语义相近 → 投影相近）+ 近邻弹簧 / 网格斥力精修；
//!   之后 = **增量**：旧坐标原样保留，新文档落到前 3 个近邻的加权质心旁；
//! - 区域：坐标上的确定性 k-means，区域名取簇内"独有度"最高的两个词；
//! - 全程确定性：同一语料两次构建坐标逐位一致（不依赖 HashMap 遍历序、不用随机数）。

use std::collections::{BTreeMap, HashMap};

/// 每篇文档保留的词数（稀疏向量长度上限）。
pub const TERMS_PER_DOC: usize = 160;
/// 近邻个数（布局弹簧与关联线都从这里取）。
pub const KNN: usize = 8;
/// 典型节点间距（世界坐标单位 ≈ 1 倍缩放下的像素）。
pub const SPACING: f32 = 64.0;

/// 一篇文档的稀疏向量：按词 id 升序，权重已 L2 归一。
#[derive(Clone, Debug, Default)]
pub struct SparseVec(pub Vec<(u32, f32)>);

impl SparseVec {
    pub fn dot(&self, o: &SparseVec) -> f32 {
        let (a, b) = (&self.0, &o.0);
        let (mut i, mut j, mut s) = (0, 0, 0.0f32);
        while i < a.len() && j < b.len() {
            match a[i].0.cmp(&b[j].0) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    s += a[i].1 * b[j].1;
                    i += 1;
                    j += 1;
                }
            }
        }
        s
    }
}

/// 场的切词：英文词（≥2 字符，小写）+ 中文二元组。与 rag::tokenize 不同：
/// 标题行不跳过（标题是最强的语义信号），也不收单个汉字（噪声太大）。
pub fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut prev_cjk: Option<char> = None;
    let flush = |w: &mut String, out: &mut Vec<String>| {
        if w.len() >= 2 && !w.chars().all(|c| c.is_ascii_digit() || c == '-' || c == '_') {
            out.push(std::mem::take(w));
        } else {
            w.clear();
        }
    };
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || ((c == '-' || c == '_') && !word.is_empty()) {
            word.push(c.to_ascii_lowercase());
            prev_cjk = None;
        } else if is_cjk(c) {
            flush(&mut word, &mut out);
            if let Some(p) = prev_cjk {
                let mut s = String::with_capacity(6);
                s.push(p);
                s.push(c);
                out.push(s);
            }
            prev_cjk = Some(c);
        } else {
            flush(&mut word, &mut out);
            prev_cjk = None;
        }
    }
    flush(&mut word, &mut out);
    out
}

/// 与 [`tokens`] 同规则，但不分配：每个词以 &str 回调（建索引的热路径用它）。
pub fn each_token(text: &str, mut f: impl FnMut(&str)) {
    let mut word = String::new();
    let mut bi = String::with_capacity(8);
    let mut prev_cjk: Option<char> = None;
    let flush = |w: &mut String, f: &mut dyn FnMut(&str)| {
        if w.len() >= 2 && !w.chars().all(|c| c.is_ascii_digit() || c == '-' || c == '_') {
            f(w);
        }
        w.clear();
    };
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || ((c == '-' || c == '_') && !word.is_empty()) {
            word.push(c.to_ascii_lowercase());
            prev_cjk = None;
        } else if is_cjk(c) {
            flush(&mut word, &mut f);
            if let Some(p) = prev_cjk {
                bi.clear();
                bi.push(p);
                bi.push(c);
                f(&bi);
            }
            prev_cjk = Some(c);
        } else {
            flush(&mut word, &mut f);
            prev_cjk = None;
        }
    }
    flush(&mut word, &mut f);
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF)
}



/// 语料的向量化结果。
pub struct Vectors {
    pub paths: Vec<String>,
    pub vecs: Vec<SparseVec>,
    /// 词表（id → 词），按字典序，保证 id 稳定。
    pub vocab: Vec<String>,
    pub idf: Vec<f32>,
    pub df: Vec<u32>,
    term_id: HashMap<String, u32>,
}

impl Vectors {
    /// `docs` = (路径, 标题, 正文)。路径排序后处理，结果与输入顺序无关。
    pub fn build(docs: &[(String, String, String)]) -> Self {
        let mut order: Vec<usize> = (0..docs.len()).collect();
        order.sort_by(|&a, &b| docs[a].0.cmp(&docs[b].0));
        let n = docs.len().max(1) as f32;
        // 1) 词频：各线程各自分配临时词号，再按词合并。标题 ×3；加法只有小整数，与顺序、线程数都无关
        let parts: Vec<(HashMap<String, u32>, Vec<Vec<(u32, f32)>>)> = par_map(order.len(), |range| {
            let mut local: HashMap<String, u32> = HashMap::new();
            let mut out: Vec<Vec<(u32, f32)>> = Vec::with_capacity(range.len());
            let mut tf: HashMap<u32, f32> = HashMap::new();
            for &i in &order[range] {
                let (_, title, body) = &docs[i];
                tf.clear();
                let mut add = |t: &str, w: f32, tf: &mut HashMap<u32, f32>| {
                    let id = match local.get(t) {
                        Some(&id) => id,
                        None => {
                            let id = local.len() as u32;
                            local.insert(t.to_string(), id);
                            id
                        }
                    };
                    *tf.entry(id).or_insert(0.0) += w;
                };
                each_token(title, |t| add(t, 3.0, &mut tf));
                each_token(body, |t| add(t, 1.0, &mut tf));
                let mut v: Vec<(u32, f32)> = tf.iter().map(|(&k, &c)| (k, c)).collect();
                v.sort_by_key(|x| x.0);
                out.push(v);
            }
            vec![(local, out)]
        });
        let mut tmp: HashMap<String, u32> = HashMap::new();
        let mut tmp_df: Vec<u32> = Vec::new();
        let mut tfs: Vec<Vec<(u32, f32)>> = Vec::with_capacity(docs.len());
        for (local, out) in parts {
            let mut map: Vec<u32> = vec![0; local.len()];
            for (t, lid) in local {
                let gid = *tmp.entry(t).or_insert_with(|| {
                    tmp_df.push(0);
                    (tmp_df.len() - 1) as u32
                });
                map[lid as usize] = gid;
            }
            for v in out {
                let mut g: Vec<(u32, f32)> = v.into_iter().map(|(k, c)| (map[k as usize], c)).collect();
                g.sort_by_key(|x| x.0);
                for &(k, _) in &g {
                    tmp_df[k as usize] += 1;
                }
                tfs.push(g);
            }
        }
        // 2) 词表：只在 ≥2 篇出现（小库除外）且不在 >60% 篇里出现的词；按字典序定最终词号
        let small = docs.len() < 8;
        let max_df = ((n * 0.6).ceil() as u32).max(2);
        let mut kept: Vec<(&String, u32)> = tmp
            .iter()
            .filter(|(_, &id)| {
                let d = tmp_df[id as usize];
                (small || d >= 2) && (small || d <= max_df)
            })
            .map(|(t, &id)| (t, id))
            .collect();
        kept.sort_by(|a, b| a.0.cmp(b.0));
        let mut remap: Vec<u32> = vec![u32::MAX; tmp.len()];
        let mut vocab = Vec::with_capacity(kept.len());
        let mut df = Vec::with_capacity(kept.len());
        for (fid, (t, id)) in kept.iter().enumerate() {
            remap[*id as usize] = fid as u32;
            vocab.push((*t).clone());
            df.push(tmp_df[*id as usize]);
        }
        let term_id: HashMap<String, u32> = vocab.iter().enumerate().map(|(i, t)| (t.clone(), i as u32)).collect();
        let idf: Vec<f32> = df.iter().map(|&d| (1.0 + n / (1.0 + d as f32)).ln()).collect();
        // 3) 向量：次线性 tf × idf，截前 TERMS_PER_DOC，L2 归一
        let vecs: Vec<SparseVec> = par_map(tfs.len(), |range| {
          let mut vecs = Vec::with_capacity(range.len());
          for tf in &tfs[range] {
            let mut v: Vec<(u32, f32)> = tf
                .iter()
                .filter_map(|&(k, c)| {
                    let id = remap[k as usize];
                    (id != u32::MAX).then(|| (id, (1.0 + c.ln()) * idf[id as usize]))
                })
                .collect();
            v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
            v.truncate(TERMS_PER_DOC);
            normalize(&mut v);
            v.sort_by_key(|x| x.0);
            vecs.push(SparseVec(v));
          }
          vecs
        });
        Vectors {
            paths: order.iter().map(|&i| docs[i].0.clone()).collect(),
            vecs,
            vocab,
            idf,
            df,
            term_id,
        }
    }

    /// 把任意一段文字投影进同一个向量空间（随手记落位、显色打分用）。
    pub fn embed(&self, text: &str) -> SparseVec {
        let mut tf: BTreeMap<u32, f32> = BTreeMap::new();
        for t in tokens(text) {
            if let Some(&id) = self.term_id.get(&t) {
                *tf.entry(id).or_insert(0.0) += 1.0;
            }
        }
        let mut v: Vec<(u32, f32)> = tf.into_iter().map(|(id, c)| (id, (1.0 + c.ln()) * self.idf[id as usize])).collect();
        normalize(&mut v);
        SparseVec(v)
    }

    /// 与 `q` 最相近的前 k 篇（余弦 > 0）。
    pub fn nearest(&self, q: &SparseVec, k: usize) -> Vec<(usize, f32)> {
        let mut s: Vec<(usize, f32)> = self.vecs.iter().enumerate().map(|(i, v)| (i, q.dot(v))).filter(|x| x.1 > 0.0).collect();
        s.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
        s.truncate(k);
        s
    }

    /// 每篇的前 k 个近邻（倒排表候选 + 精确点积）。
    pub fn knn(&self, k: usize) -> Vec<Vec<(usize, f32)>> {
        let n = self.vecs.len();
        // 候选生成只用"有区分度"的词：df 上限随库规模放宽
        let cap = (n / 20).clamp(40, 200) as u32;
        let mut posting: Vec<Vec<(u32, f32)>> = vec![Vec::new(); self.vocab.len()];
        for (i, v) in self.vecs.iter().enumerate() {
            for &(t, w) in &v.0 {
                if self.df[t as usize] <= cap {
                    posting[t as usize].push((i as u32, w));
                }
            }
        }
        let posting = &posting;
        par_map(n, |range| {
        let mut out = Vec::with_capacity(range.len());
        let mut acc: Vec<f32> = vec![0.0; n];
        let mut touched: Vec<u32> = Vec::new();
        for i in range {
            let v = &self.vecs[i];
            for &(t, w) in &v.0 {
                for &(j, wj) in &posting[t as usize] {
                    if j as usize == i {
                        continue;
                    }
                    if acc[j as usize] == 0.0 {
                        touched.push(j);
                    }
                    acc[j as usize] += w * wj;
                }
            }
            // 候选取前 4k，再用完整点积精排（常见词也计入相似度）
            let cmp = |a: &u32, b: &u32| acc[*b as usize].partial_cmp(&acc[*a as usize]).unwrap_or(std::cmp::Ordering::Equal).then(a.cmp(b));
            let take = (k * 4).min(touched.len());
            if take > 0 && take < touched.len() {
                touched.select_nth_unstable_by(take - 1, cmp);
            }
            let mut best: Vec<(usize, f32)> = touched[..take].iter().map(|&j| (j as usize, v.dot(&self.vecs[j as usize]))).collect();
            best.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
            best.truncate(k);
            out.push(best);
            for &j in &touched {
                acc[j as usize] = 0.0;
            }
            touched.clear();
        }
        out
        })
    }
}

/// 按下标区间切块并行，结果按原顺序拼回（每块独立计算，输出与线程数无关）。
fn par_map<T: Send>(n: usize, f: impl Fn(std::ops::Range<usize>) -> Vec<T> + Sync) -> Vec<T> {
    let threads = std::thread::available_parallelism().map(|x| x.get()).unwrap_or(1).min(8);
    if threads <= 1 || n < 512 {
        return f(0..n);
    }
    let chunk = n.div_ceil(threads);
    let f = &f;
    std::thread::scope(|sc| {
        let hs: Vec<_> = (0..threads).map(|t| sc.spawn(move || f((t * chunk).min(n)..((t + 1) * chunk).min(n)))).collect();
        hs.into_iter().flat_map(|h| h.join().expect("field worker")).collect()
    })
}

fn normalize(v: &mut [(u32, f32)]) {
    let n: f32 = v.iter().map(|x| x.1 * x.1).sum::<f32>().sqrt();
    if n > 1e-9 {
        for x in v.iter_mut() {
            x.1 /= n;
        }
    }
}

/// 稳定的 64 位哈希（FNV-1a）：不随进程变化，用于确定性的"随机"。
pub fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 由哈希得到 [-1, 1) 的伪高斯值（三个均匀数求和）。
fn hnorm(h: u64) -> f32 {
    let a = (h & 0x1fffff) as f32 / 2097152.0;
    let b = ((h >> 21) & 0x1fffff) as f32 / 2097152.0;
    let c = ((h >> 42) & 0x1fffff) as f32 / 2097152.0;
    (a + b + c) / 1.5 - 1.0
}

/// 布局：`prev` 里已有坐标的文档原样保留；`full` = true 时整体重排。
/// 返回与 `vectors.paths` 同序的坐标。
pub fn layout(v: &Vectors, knn: &[Vec<(usize, f32)>], prev: &HashMap<String, (f32, f32)>, full: bool) -> Vec<(f32, f32)> {
    let n = v.paths.len();
    if n == 0 {
        return Vec::new();
    }
    let radius = (n as f32 / std::f32::consts::PI).sqrt() * SPACING * 1.15;
    let mut pos: Vec<Option<(f32, f32)>> = v.paths.iter().map(|p| if full { None } else { prev.get(p).copied() }).collect();
    let fresh_start = pos.iter().all(|p| p.is_none());
    if fresh_start {
        // 哈希随机投影：x = Σ w·a(t)，y = Σ w·b(t)。语义相近的文档投影相近。
        let mut raw: Vec<(f32, f32)> = v
            .vecs
            .iter()
            .map(|sv| {
                let (mut x, mut y) = (0.0f32, 0.0f32);
                for &(t, w) in &sv.0 {
                    let h = fnv(&v.vocab[t as usize]);
                    x += w * hnorm(h);
                    y += w * hnorm(h.rotate_left(29) ^ 0x9e3779b97f4a7c15);
                }
                (x, y)
            })
            .collect();
        let (mx, my) = mean(&raw);
        let sd = (raw.iter().map(|p| (p.0 - mx).powi(2) + (p.1 - my).powi(2)).sum::<f32>() / n as f32).sqrt().max(1e-6);
        for p in raw.iter_mut() {
            *p = ((p.0 - mx) / sd * radius * 0.5, (p.1 - my) / sd * radius * 0.5);
        }
        // 投影出来中心极密（近似高斯）：按到中心的距离排名，把半径摊匀到整张圆盘（角度不变，远近次序不变）。
        // 否则前几十轮斥力在中心格子里是 O(n²)，1.2 万篇要十几秒。
        let mut by_r: Vec<usize> = (0..n).collect();
        by_r.sort_by(|&a, &b| {
            let ra = raw[a].0 * raw[a].0 + raw[a].1 * raw[a].1;
            let rb = raw[b].0 * raw[b].0 + raw[b].1 * raw[b].1;
            ra.partial_cmp(&rb).unwrap_or(std::cmp::Ordering::Equal).then(a.cmp(&b))
        });
        for (rank, &i) in by_r.iter().enumerate() {
            let (x, y) = raw[i];
            let r0 = (x * x + y * y).sqrt();
            let r1 = radius * 0.92 * ((rank as f32 + 0.5) / n as f32).sqrt();
            raw[i] = if r0 > 1e-6 { (x / r0 * r1, y / r0 * r1) } else { (r1, 0.0) };
        }
        let mut pts = raw;
        relax(&mut pts, knn, &vec![true; n], 260, radius);
        return pts;
    }
    // 增量：新文档落到已有近邻的加权质心旁，再只让新文档做局部松弛
    let mut movable = vec![false; n];
    for i in 0..n {
        if pos[i].is_some() {
            continue;
        }
        movable[i] = true;
        let (mut x, mut y, mut w) = (0.0, 0.0, 0.0);
        for &(j, s) in knn[i].iter().take(3) {
            if let Some((px, py)) = pos[j] {
                x += px * s;
                y += py * s;
                w += s;
            }
        }
        let h = fnv(&v.paths[i]);
        let ang = (h % 6283) as f32 / 1000.0;
        let r = SPACING * 0.55;
        pos[i] = Some(if w > 0.0 {
            (x / w + ang.cos() * r, y / w + ang.sin() * r)
        } else {
            // 没有近邻：放到场的外圈，不压在别人身上
            (ang.cos() * radius * 1.1, ang.sin() * radius * 1.1)
        });
    }
    let mut pts: Vec<(f32, f32)> = pos.into_iter().map(|p| p.unwrap()).collect();
    if movable.iter().any(|&m| m) {
        relax(&mut pts, knn, &movable, 60, radius);
    }
    pts
}

fn mean(p: &[(f32, f32)]) -> (f32, f32) {
    let n = p.len().max(1) as f32;
    (p.iter().map(|x| x.0).sum::<f32>() / n, p.iter().map(|x| x.1).sum::<f32>() / n)
}

/// 每个格子参与斥力计算的点数上限（超过就等距抽样）。
const REPEL_CAP: usize = 24;

/// 力导向精修：近邻弹簧 + 网格分桶的短程斥力 + 轻微向心力。确定性（固定迭代序）。
fn relax(pts: &mut [(f32, f32)], knn: &[Vec<(usize, f32)>], movable: &[bool], iters: usize, radius: f32) {
    let n = pts.len();
    let cell = SPACING * 1.6;
    let mut disp = vec![(0.0f32, 0.0f32); n];
    let mut cells = vec![(0i32, 0i32); n];
    let (mut start, mut fill, mut order): (Vec<usize>, Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new(), Vec::new());
    let mut indeg = vec![0usize; n];
    for nb in knn.iter() {
        for &(j, _) in nb {
            indeg[j] += 1;
        }
    }
    let inv_deg: Vec<f32> = indeg.iter().map(|&d| 1.0 / (d.max(1) as f32).sqrt().max(1.0)).collect();
    for it in 0..iters {
        let temp = SPACING * 0.9 * (1.0 - it as f32 / iters as f32) + 1.0;
        for d in disp.iter_mut() {
            *d = (0.0, 0.0);
        }
        // 斥力：同格与相邻格。稠密网格 + 计数排序（同格内按下标升序，与逐个 push 的顺序一致，结果逐位不变）
        let (mut gx0, mut gy0, mut gx1, mut gy1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for (i, p) in pts.iter().enumerate() {
            let c = ((p.0 / cell).floor() as i32, (p.1 / cell).floor() as i32);
            cells[i] = c;
            gx0 = gx0.min(c.0);
            gy0 = gy0.min(c.1);
            gx1 = gx1.max(c.0);
            gy1 = gy1.max(c.1);
        }
        let gw = (gx1 - gx0 + 1).max(1) as usize;
        let gh = (gy1 - gy0 + 1).max(1) as usize;
        let slot = |c: (i32, i32)| (c.1 - gy0) as usize * gw + (c.0 - gx0) as usize;
        start.clear();
        start.resize(gw * gh + 1, 0);
        for c in cells.iter() {
            start[slot(*c) + 1] += 1;
        }
        for k in 1..start.len() {
            start[k] += start[k - 1];
        }
        fill.clear();
        fill.extend_from_slice(&start[..gw * gh]);
        order.resize(n, 0);
        for (i, c) in cells.iter().enumerate() {
            let s = slot(*c);
            order[fill[s]] = i;
            fill[s] += 1;
        }
        // 每个点的斥力只读坐标、只写自己：按区间并行，结果与线程数无关
        let rep: Vec<(f32, f32)> = {
            let (pts, cells, order, start) = (&*pts, &cells, &order, &start);
            par_map(n, |range| {
                range
                    .map(|i| {
                        let mut acc = (0.0f32, 0.0f32);
                        if !movable[i] {
                            return acc;
                        }
                        let (gx, gy) = cells[i];
                        for dx in -1..=1 {
                            for dy in -1..=1 {
                                let (cx, cy) = (gx + dx, gy + dy);
                                if cx < gx0 || cx > gx1 || cy < gy0 || cy > gy1 {
                                    continue;
                                }
                                let s = slot((cx, cy));
                                let list = &order[start[s]..start[s + 1]];
                                // 挤成一团的格子：等距抽样，力按步长放大（总量近似不变）。代价封顶，挤得越密推得越开
                                let step = list.len().div_ceil(REPEL_CAP).max(1);
                                let mass = step as f32;
                                for &j in list.iter().step_by(step) {
                                    if j == i {
                                        continue;
                                    }
                                    let (mut ddx, mut ddy) = (pts[i].0 - pts[j].0, pts[i].1 - pts[j].1);
                                    let mut d2 = ddx * ddx + ddy * ddy;
                                    if d2 < 1e-4 {
                                        let h = fnv(&format!("{i}:{j}"));
                                        ddx = hnorm(h);
                                        ddy = hnorm(h >> 7);
                                        d2 = ddx * ddx + ddy * ddy + 1e-4;
                                    }
                                    let d = d2.sqrt();
                                    if d < cell {
                                        let f = SPACING * SPACING / d2.max(SPACING * SPACING * 0.04) * 0.9 * mass;
                                        acc.0 += ddx / d * f;
                                        acc.1 += ddy / d * f;
                                    }
                                }
                            }
                        }
                        acc
                    })
                    .collect()
            })
        };
        disp.copy_from_slice(&rep);
        // 弹簧：近邻越像，静长越短
        for i in 0..n {
            for &(j, s) in &knn[i] {
                let (dx, dy) = (pts[j].0 - pts[i].0, pts[j].1 - pts[i].1);
                let d = (dx * dx + dy * dy).sqrt().max(1e-3);
                let rest = SPACING * (0.75 + 1.4 * (1.0 - s.clamp(0.0, 1.0)));
                // 拉力封顶：一篇被很多远处的近邻同时拉时不至于把整簇吸成实心球（大库里斥力只看邻格，长程拉力会压倒它）
                let f = (d - rest).clamp(-SPACING, SPACING * 1.5) * 0.06 * (0.3 + s);
                if movable[i] {
                    disp[i].0 += dx / d * f;
                    disp[i].1 += dy / d * f;
                }
                if movable[j] {
                    // 被很多篇当近邻的「枢纽」只按入度的平方根受力，否则整簇会被吸成一团
                    let g = f * inv_deg[j];
                    disp[j].0 -= dx / d * g;
                    disp[j].1 -= dy / d * g;
                }
            }
        }
        // 向心：把孤点轻轻拉回，防止飘散
        for i in 0..n {
            if movable[i] {
                let r = (pts[i].0 * pts[i].0 + pts[i].1 * pts[i].1).sqrt();
                if r > radius {
                    disp[i].0 -= pts[i].0 / r * (r - radius) * 0.08;
                    disp[i].1 -= pts[i].1 / r * (r - radius) * 0.08;
                }
            }
        }
        for i in 0..n {
            if !movable[i] {
                continue;
            }
            let (dx, dy) = disp[i];
            let d = (dx * dx + dy * dy).sqrt();
            if d > 1e-6 {
                let m = d.min(temp);
                pts[i].0 += dx / d * m;
                pts[i].1 += dy / d * m;
            }
        }
    }
}

/// 区域：坐标上的确定性 k-means（最远点初始化）。返回每篇的区域号与区域数。
pub fn regions(pts: &[(f32, f32)]) -> (Vec<usize>, usize) {
    let n = pts.len();
    if n == 0 {
        return (Vec::new(), 0);
    }
    let k = ((n as f32).sqrt() / 1.6).round().clamp(1.0, 14.0) as usize;
    let k = k.min(n);
    let (cx, cy) = mean(pts);
    // 初始化：离质心最远的点，然后依次取离已选中心最远的点
    let mut cent: Vec<(f32, f32)> = Vec::with_capacity(k);
    let far = (0..n).max_by(|&a, &b| d2(pts[a], (cx, cy)).partial_cmp(&d2(pts[b], (cx, cy))).unwrap().then(b.cmp(&a))).unwrap();
    cent.push(pts[far]);
    while cent.len() < k {
        let nxt = (0..n)
            .max_by(|&a, &b| {
                let da = cent.iter().map(|c| d2(pts[a], *c)).fold(f32::MAX, f32::min);
                let db = cent.iter().map(|c| d2(pts[b], *c)).fold(f32::MAX, f32::min);
                da.partial_cmp(&db).unwrap().then(b.cmp(&a))
            })
            .unwrap();
        cent.push(pts[nxt]);
    }
    let mut asg = vec![0usize; n];
    for _ in 0..40 {
        let mut changed = false;
        for i in 0..n {
            let best = (0..k).min_by(|&a, &b| d2(pts[i], cent[a]).partial_cmp(&d2(pts[i], cent[b])).unwrap().then(a.cmp(&b))).unwrap();
            if asg[i] != best {
                asg[i] = best;
                changed = true;
            }
        }
        let mut sum = vec![(0.0f32, 0.0f32, 0usize); k];
        for i in 0..n {
            sum[asg[i]].0 += pts[i].0;
            sum[asg[i]].1 += pts[i].1;
            sum[asg[i]].2 += 1;
        }
        for c in 0..k {
            if sum[c].2 > 0 {
                cent[c] = (sum[c].0 / sum[c].2 as f32, sum[c].1 / sum[c].2 as f32);
            }
        }
        if !changed {
            break;
        }
    }
    // 重新编号：按区域人数从多到少，号码稳定
    let mut count = vec![0usize; k];
    for &a in &asg {
        count[a] += 1;
    }
    let mut ord: Vec<usize> = (0..k).filter(|&c| count[c] > 0).collect();
    ord.sort_by(|&a, &b| count[b].cmp(&count[a]).then(a.cmp(&b)));
    let remap: HashMap<usize, usize> = ord.iter().enumerate().map(|(i, &c)| (c, i)).collect();
    (asg.iter().map(|a| remap[a]).collect(), ord.len())
}

fn d2(a: (f32, f32), b: (f32, f32)) -> f32 {
    (a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)
}

/// 区域名：取区域里的"中心文档"（与区域质心向量最像的一篇）的短标题。
/// 二元组切词给不出好词（"件系""时两"这类跨词片段），而中心文档的标题是人写的、读得懂，
/// 也正好回答"这一片在讲什么"。重名时退到第二中心的文档。
pub fn region_names(v: &Vectors, asg: &[usize], k: usize, titles: &[String]) -> Vec<String> {
    let mut members: Vec<Vec<usize>> = vec![Vec::new(); k];
    for (i, &c) in asg.iter().enumerate() {
        members[c].push(i);
    }
    let mut used: std::collections::HashSet<String> = Default::default();
    (0..k)
        .map(|c| {
            let m = &members[c];
            // 区域质心向量（稀疏和）；与质心点积最大的成员 = 中心文档。O(成员数 × 词数)，大库也是毫秒级
            let mut centroid: BTreeMap<u32, f32> = BTreeMap::new();
            for &i in m {
                for &(t, w) in &v.vecs[i].0 {
                    *centroid.entry(t).or_insert(0.0) += w;
                }
            }
            let cv = SparseVec(centroid.into_iter().collect());
            let mut rank: Vec<(usize, f32)> = m.iter().map(|&i| (i, v.vecs[i].dot(&cv))).collect();
            rank.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
            for (i, _) in &rank {
                let t = short_title(titles.get(*i).map(|s| s.as_str()).unwrap_or(""));
                if !t.is_empty() && used.insert(t.clone()) {
                    return t;
                }
            }
            format!("区域 {}", c + 1)
        })
        .collect()
}

/// 标题的主干：去掉破折号/冒号/括号之后的副标题与「Spec」后缀，最长 16 个字。
pub fn short_title(t: &str) -> String {
    let t = t.trim().trim_start_matches("Litmus").trim();
    let cut = t
        .char_indices()
        .find(|(i, ch)| *i > 0 && matches!(ch, '—' | '–' | ':' | '：' | '（' | '(' | '|' | '·' | '，'))
        .map(|(i, _)| i)
        .unwrap_or(t.len());
    let head = t[..cut].trim().trim_end_matches(['-', ' ']);
    // 「ADR-0002 — 文件为内容真相」这类编号打头的标题：编号说明不了内容，取后半
    if cut < t.len() && !head.chars().any(is_cjk) && head.chars().any(|c| c.is_ascii_digit()) {
        let rest = t[cut..].trim_start_matches(|c: char| matches!(c, '—' | '–' | ':' | '：' | '|' | '·' | '，') || c.is_whitespace());
        if !rest.is_empty() {
            return short_title(rest);
        }
    }
    let head = head.strip_suffix(" Spec").unwrap_or(head).trim();
    if head.chars().count() <= 16 {
        return head.to_string();
    }
    // 过长：西文退到词边界，中文直接截
    let s: String = head.chars().take(16).collect();
    match s.rfind(' ') {
        Some(i) if !s.chars().any(is_cjk) && i > 4 => s[..i].to_string(),
        _ => s,
    }
}


/// 关联线：每篇与最像的两篇相连（相似度 ≥ 0.12），去重。
pub fn edges(knn: &[Vec<(usize, f32)>]) -> Vec<(usize, usize, f32)> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for (i, nb) in knn.iter().enumerate() {
        for &(j, s) in nb.iter().take(2) {
            if s < 0.12 {
                continue;
            }
            let key = (i.min(j), i.max(j));
            if seen.insert(key) {
                out.push((key.0, key.1, s));
            }
        }
    }
    out
}

/// 一次完整的场构建结果。
pub struct Field {
    pub vectors: Vectors,
    pub pos: Vec<(f32, f32)>,
    pub region: Vec<usize>,
    pub region_names: Vec<String>,
    pub edges: Vec<(usize, usize, f32)>,
}

/// 构建场。`prev` 来自 `.litmus/local/field.json`（可为空）；`full` 强制整体重排。
pub fn build(docs: &[(String, String, String)], prev: &HashMap<String, (f32, f32)>, full: bool) -> Field {
    let vectors = Vectors::build(docs);
    let knn = vectors.knn(KNN);
    // 变动超过三成：增量会把新内容挤在旧版图的缝里，不如整体重排
    let kept = vectors.paths.iter().filter(|p| prev.contains_key(*p)).count();
    let full = full || (vectors.paths.len() >= 10 && kept * 10 < vectors.paths.len() * 7);
    let pos = layout(&vectors, &knn, prev, full);
    let (region, k) = regions(&pos);
    let titles: Vec<String> = {
        let m: HashMap<&str, &str> = docs.iter().map(|(p, t, _)| (p.as_str(), t.as_str())).collect();
        vectors.paths.iter().map(|p| m.get(p.as_str()).copied().unwrap_or("").to_string()).collect()
    };
    let region_names = region_names(&vectors, &region, k, &titles);
    let edges = edges(&knn);
    Field { vectors, pos, region, region_names, edges }
}

/// 一段文字在场里应该落在哪：前 3 近邻的加权质心。返回 (x, y, 近邻下标与相似度)。
pub fn place(f: &Field, text: &str) -> Option<(f32, f32, Vec<(usize, f32)>)> {
    let q = f.vectors.embed(text);
    let nb = f.vectors.nearest(&q, 3);
    if nb.is_empty() {
        return None;
    }
    let (mut x, mut y, mut w) = (0.0, 0.0, 0.0);
    for &(i, s) in &nb {
        x += f.pos[i].0 * s;
        y += f.pos[i].1 * s;
        w += s;
    }
    let h = fnv(text);
    let ang = (h % 6283) as f32 / 1000.0;
    Some((x / w + ang.cos() * SPACING * 0.55, y / w + ang.sin() * SPACING * 0.55, nb))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> Vec<(String, String, String)> {
        let topics = [
            ("同步", "同步协议 冲突 合并 CRDT automerge 离线 设备 op-log 并发"),
            ("发布", "发布 站点 部署 线上 静态 构建 git pages 上传 域名"),
            ("设计", "设计 配色 字体 圆角 阴影 token 视觉 层级 对比度 主题"),
            ("检索", "检索 搜索 索引 向量 召回 排序 bm25 fts 语义 混合"),
        ];
        let mut out = Vec::new();
        for (ti, (name, words)) in topics.iter().enumerate() {
            for k in 0..8 {
                let body = format!("# {name}笔记 {k}\n\n{words} {words} 第{k}篇 编号{ti}{k}");
                out.push((format!("Notes/{name}/{k:02}.md"), format!("{name}笔记 {k}"), body));
            }
        }
        out
    }

    #[test]
    fn tokens_mix_words_and_cjk_bigrams() {
        let t = tokens("用 CRDT 做同步");
        assert!(t.contains(&"crdt".to_string()));
        assert!(t.contains(&"同步".to_string()));
        assert!(!t.contains(&"用".to_string()), "不收单字");
    }

    #[test]
    fn same_corpus_same_coordinates() {
        let c = corpus();
        let a = build(&c, &HashMap::new(), true);
        let mut rev = c.clone();
        rev.reverse();
        let b = build(&rev, &HashMap::new(), true);
        assert_eq!(a.vectors.paths, b.vectors.paths);
        assert_eq!(a.pos, b.pos, "输入顺序不同，坐标必须逐位一致");
        assert_eq!(a.region, b.region);
    }

    #[test]
    fn similar_documents_land_close() {
        let f = build(&corpus(), &HashMap::new(), true);
        let idx = |p: &str| f.vectors.paths.iter().position(|x| x == p).unwrap();
        let (a, b, z) = (idx("Notes/同步/00.md"), idx("Notes/同步/05.md"), idx("Notes/发布/00.md"));
        let d = |i: usize, j: usize| ((f.pos[i].0 - f.pos[j].0).powi(2) + (f.pos[i].1 - f.pos[j].1).powi(2)).sqrt();
        assert!(d(a, b) < d(a, z), "同主题应比跨主题更近：{} vs {}", d(a, b), d(a, z));
        // 同一主题的 8 篇大多落在同一区域
        let r0 = f.region[a];
        let same = (0..8).filter(|k| f.region[idx(&format!("Notes/同步/{k:02}.md"))] == r0).count();
        assert!(same >= 6, "同主题分散到了多个区域：{same}/8");
    }

    #[test]
    fn adding_one_document_moves_nothing_else() {
        let c = corpus();
        let a = build(&c, &HashMap::new(), true);
        let prev: HashMap<String, (f32, f32)> = a.vectors.paths.iter().cloned().zip(a.pos.iter().cloned()).collect();
        let mut c2 = c.clone();
        c2.push(("Notes/同步/新.md".into(), "离线同步".into(), "离线 同步 冲突 合并 CRDT".into()));
        let b = build(&c2, &prev, false);
        for (i, p) in b.vectors.paths.iter().enumerate() {
            if let Some(old) = prev.get(p) {
                assert_eq!(*old, b.pos[i], "旧文档 {p} 被挪动了");
            }
        }
        let ni = b.vectors.paths.iter().position(|p| p == "Notes/同步/新.md").unwrap();
        let si = b.vectors.paths.iter().position(|p| p == "Notes/同步/00.md").unwrap();
        let fi = b.vectors.paths.iter().position(|p| p == "Notes/发布/00.md").unwrap();
        let d = |i: usize, j: usize| ((b.pos[i].0 - b.pos[j].0).powi(2) + (b.pos[i].1 - b.pos[j].1).powi(2)).sqrt();
        assert!(d(ni, si) < d(ni, fi), "新文档应落在同步一侧");
    }

    #[test]
    fn short_title_drops_subtitles() {
        assert_eq!(short_title("Litmus 设计宪法 — 一个 AI 时代的知识操作系统"), "设计宪法");
        assert_eq!(short_title("ADR-0002 — 文件为内容真相，op-log 为并发真相"), "文件为内容真相");
        assert_eq!(short_title("竞争定位（替代 Obsidian）"), "竞争定位");
        assert_eq!(short_title("Consistency Spec — 文件真相"), "Consistency");
    }

    #[test]
    fn region_names_are_distinct_words() {
        let f = build(&corpus(), &HashMap::new(), true);
        assert!(f.region_names.len() >= 2);
        for n in &f.region_names {
            assert!(!n.is_empty());
            assert!(n.contains("笔记"), "区域名应取自成员标题：{n}");
        }
        let mut s = f.region_names.clone();
        s.sort();
        s.dedup();
        assert_eq!(s.len(), f.region_names.len(), "区域名重复：{:?}", f.region_names);
    }

    #[test]
    fn place_puts_text_near_its_topic() {
        let f = build(&corpus(), &HashMap::new(), true);
        let (x, y, nb) = place(&f, "检索 向量 召回").unwrap();
        assert!(f.vectors.paths[nb[0].0].starts_with("Notes/检索/"));
        let si = f.vectors.paths.iter().position(|p| p == "Notes/检索/00.md").unwrap();
        let fi = f.vectors.paths.iter().position(|p| p == "Notes/设计/00.md").unwrap();
        let d = |i: usize| ((f.pos[i].0 - x).powi(2) + (f.pos[i].1 - y).powi(2)).sqrt();
        assert!(d(si) < d(fi));
    }

    #[test]
    fn empty_and_tiny_libraries_do_not_panic() {
        let f = build(&[], &HashMap::new(), true);
        assert!(f.pos.is_empty());
        let one = vec![("Notes/a.md".to_string(), "A".to_string(), "只有一篇".to_string())];
        let f = build(&one, &HashMap::new(), true);
        assert_eq!(f.pos.len(), 1);
        assert_eq!(f.region, vec![0]);
    }
}
