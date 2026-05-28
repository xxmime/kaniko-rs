//! Benchmark: OCI layer creation performance.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use oci_image::layer::Layer;
use std::fs;
use tempfile::TempDir;

fn bench_layer_from_tar_small(c: &mut Criterion) {
    let tmp = TempDir::new().unwrap();
    for i in 0..5 {
        fs::write(tmp.path().join(format!("file{}.txt", i)), "hello world\n".repeat(100)).unwrap();
    }

    // Build a tar from the temp directory
    let mut tar_data = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_data);
        for entry in fs::read_dir(tmp.path()).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                builder.append_path_with_name(entry.path(), entry.file_name()).unwrap();
            }
        }
        builder.finish().unwrap();
    }

    c.bench_function("layer_from_tar_5_files", |b| {
        b.iter(|| Layer::from_tar_uncompressed(black_box(tar_data.clone())).unwrap())
    });
}

fn bench_layer_from_tar_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("layer_from_tar_scaling");
    for file_count in [10, 50, 100] {
        let tmp = TempDir::new().unwrap();
        let mut tar_data = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_data);
            for i in 0..file_count {
                let file_path = tmp.path().join(format!("file_{:04}.txt", i));
                fs::write(&file_path, format!("content of file {}\n", i).repeat(50)).unwrap();
                builder.append_path_with_name(&file_path, format!("file_{:04}.txt", i)).unwrap();
            }
            builder.finish().unwrap();
        }

        group.bench_with_input(
            BenchmarkId::from_parameter(file_count),
            &tar_data,
            |b, tar_data| {
                b.iter(|| Layer::from_tar_uncompressed(black_box(tar_data.clone())).unwrap())
            },
        );
    }
    group.finish();
}

fn bench_layer_from_bytes(c: &mut Criterion) {
    // Simulate a pre-compressed layer
    let data = vec![0u8; 1024 * 100]; // 100KB
    c.bench_function("layer_from_bytes_100kb", |b| {
        b.iter(|| Layer::from_bytes(black_box(data.clone()), "application/vnd.oci.image.layer.v1.tar+gzip").unwrap())
    });
}

criterion_group!(benches, bench_layer_from_tar_small, bench_layer_from_tar_scaling, bench_layer_from_bytes);
criterion_main!(benches);