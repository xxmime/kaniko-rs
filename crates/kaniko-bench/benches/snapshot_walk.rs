//! Benchmark: Snapshot filesystem walking performance.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use kaniko_snapshot::walker::{walk_with_ignore, IgnorePattern};
use std::fs;
use tempfile::TempDir;

fn bench_walk_no_ignore(c: &mut Criterion) {
    let tmp = TempDir::new().unwrap();
    for i in 0..100 {
        fs::write(tmp.path().join(format!("file_{:04}.txt", i)), "data\n".repeat(50)).unwrap();
    }
    let path = tmp.path().to_path_buf();

    c.bench_function("walk_100_files_no_ignore", |b| {
        b.iter(|| walk_with_ignore(black_box(&path), &[]).unwrap().len())
    });
}

fn bench_walk_with_dockerignore(c: &mut Criterion) {
    let tmp = TempDir::new().unwrap();
    // Create files, some should be ignored
    for i in 0..80 {
        fs::write(tmp.path().join(format!("src_{:04}.rs", i)), "fn main() {}").unwrap();
    }
    for i in 0..20 {
        fs::write(tmp.path().join(format!("log_{:04}.txt", i)), "log entry\n".repeat(100)).unwrap();
    }
    let ignore_patterns = vec![
        IgnorePattern { pattern: "log_*.txt".to_string(), negation: false, dir_only: false },
        IgnorePattern { pattern: "*.log".to_string(), negation: false, dir_only: false },
    ];
    let path = tmp.path().to_path_buf();

    c.bench_function("walk_100_files_with_ignore", |b| {
        b.iter(|| walk_with_ignore(black_box(&path), &ignore_patterns).unwrap().len())
    });
}

fn bench_walk_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("walk_scaling");
    for file_count in [50, 200, 500] {
        let tmp = TempDir::new().unwrap();
        for i in 0..file_count {
            fs::write(
                tmp.path().join(format!("file_{:04}.txt", i)),
                format!("content {}\n", i).repeat(20),
            ).unwrap();
        }
        let path = tmp.path().to_path_buf();
        group.bench_with_input(
            BenchmarkId::from_parameter(file_count),
            &path,
            |b, path| {
                b.iter(|| walk_with_ignore(black_box(path), &[]).unwrap().len())
            },
        );
    }
    group.finish();
}

fn bench_walk_doublestar_pattern(c: &mut Criterion) {
    let tmp = TempDir::new().unwrap();
    // Create nested directory structure
    for d in 0..3 {
        let dir = tmp.path().join(format!("dir{}", d));
        fs::create_dir_all(&dir).unwrap();
        for i in 0..10 {
            fs::write(dir.join(format!("file{}.txt", i)), "data").unwrap();
            fs::write(dir.join(format!("file{}.log", i)), "log").unwrap();
        }
    }
    let ignore_patterns = vec![
        IgnorePattern { pattern: "**/*.log".to_string(), negation: false, dir_only: false },
    ];
    let path = tmp.path().to_path_buf();

    c.bench_function("walk_doublestar_pattern", |b| {
        b.iter(|| walk_with_ignore(black_box(&path), &ignore_patterns).unwrap().len())
    });
}

criterion_group!(
    benches,
    bench_walk_no_ignore,
    bench_walk_with_dockerignore,
    bench_walk_scaling,
    bench_walk_doublestar_pattern,
);
criterion_main!(benches);