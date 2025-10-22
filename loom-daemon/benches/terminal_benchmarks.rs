use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::path::PathBuf;
use tempfile::TempDir;

fn benchmark_terminal_creation(c: &mut Criterion) {
    c.bench_function("create_temp_dir", |b| {
        b.iter(|| {
            let temp_dir = TempDir::new().unwrap();
            let path = temp_dir.path().to_path_buf();
            black_box(path);
        });
    });
}

fn benchmark_path_operations(c: &mut Criterion) {
    c.bench_function("path_join_operations", |b| {
        let base = PathBuf::from("/tmp/loom");
        b.iter(|| {
            let path = base.join("worktrees").join("issue-123");
            black_box(path);
        });
    });
}

criterion_group!(benches, benchmark_terminal_creation, benchmark_path_operations);
criterion_main!(benches);
