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
    let t3 = Instant::now();
    let (r, k) = kernel_core::field::regions(&pos);
    eprintln!("regions {:?} k={k}", t3.elapsed());
    let t4 = Instant::now();
    let titles: Vec<String> = docs.iter().map(|d| d.1.clone()).collect();
    let _ = kernel_core::field::region_names(&v, &r, k, &titles);
    eprintln!("names {:?}", t4.elapsed());
}
