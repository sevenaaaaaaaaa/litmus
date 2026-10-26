use std::collections::HashMap;
use std::time::Instant;
fn main() {
    let dir = std::env::args().nth(1).unwrap();
    let mut docs = Vec::new();
    for e in walkdir::WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()) {
        if e.path().extension().map_or(false, |x| x == "md") {
            let t = std::fs::read_to_string(e.path()).unwrap();
            docs.push((e.path().display().to_string(), t.lines().next().unwrap_or("").to_string(), t));
        }
    }
    let t0 = Instant::now();
    let v = kernel_core::field::Vectors::build(&docs);
    eprintln!("vectors {} docs {:?}", docs.len(), t0.elapsed());
    let t1 = Instant::now();
    let knn = v.knn(8);
    eprintln!("knn {:?}", t1.elapsed());
    let t2 = Instant::now();
    let pos = kernel_core::field::layout(&v, &knn, &HashMap::new(), true);
    eprintln!("layout {:?}", t2.elapsed());
    let sum: f64 = pos.iter().enumerate().map(|(i, p)| (p.0 as f64) * (i as f64 + 1.0) + (p.1 as f64) * 0.5).sum();
    eprintln!("checksum {sum:.3}");
    // 邻近保持：前 3 个语义近邻里，有几成在 2.5 个间距（160px）以内
    let (mut near, mut tot) = (0usize, 0usize);
    for (i, nb) in knn.iter().enumerate() {
        for &(j, _) in nb.iter().take(3) {
            tot += 1;
            let (dx, dy) = (pos[i].0 - pos[j].0, pos[i].1 - pos[j].1);
            if (dx * dx + dy * dy).sqrt() < 160.0 {
                near += 1;
            }
        }
    }
    eprintln!("neighbors kept {:.1}%", near as f64 * 100.0 / tot.max(1) as f64);
    // 密度体检：每 64×64 的格子最多挤了几篇（> 12 说明挤成了团，标题必然互相压住）
    let mut cells: HashMap<(i32, i32), usize> = HashMap::new();
    for p in &pos {
        *cells.entry(((p.0 / 64.0).floor() as i32, (p.1 / 64.0).floor() as i32)).or_default() += 1;
    }
    let mut occ: Vec<usize> = cells.values().copied().collect();
    occ.sort_unstable();
    let crowded: usize = occ.iter().filter(|&&c| c > 12).sum();
    eprintln!("density max/cell {} · p99 {} · docs in crowded cells {} ({:.1}%)", occ.last().unwrap_or(&0), occ[(occ.len() * 99 / 100).min(occ.len().saturating_sub(1))], crowded, crowded as f64 * 100.0 / pos.len().max(1) as f64);
    let t3 = Instant::now();
    let (r, k) = kernel_core::field::regions(&pos);
    eprintln!("regions {:?} k={k}", t3.elapsed());
    let t4 = Instant::now();
    let titles: Vec<String> = docs.iter().map(|d| d.1.clone()).collect();
    let _ = kernel_core::field::region_names(&v, &r, k, &titles);
    eprintln!("names {:?}", t4.elapsed());
}
