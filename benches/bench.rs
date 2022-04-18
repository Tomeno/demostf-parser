use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::fs;
use tf_demo_parser::{Demo, DemoParser, Stream};

fn bench_file(input_file: &str, b: &mut Criterion) {
    let file = fs::read(input_file).expect("Unable to read file");
    let demo = Demo::new(&file);
    let stream: Stream = demo.get_stream();

    b.bench_function(&format!("bench file {}", input_file), |b| {
        b.iter(|| {
            let (_, state) = DemoParser::new(stream.clone()).parse().unwrap();
            black_box(state);
        })
    });
}

fn bench_gully(b: &mut Criterion) {
    bench_file("test_data/gully.dem", b);
}

fn bench_comp(b: &mut Criterion) {
    bench_file("test_data/comp.dem", b);
}

criterion_group!(benches, bench_comp, bench_gully);
criterion_main!(benches);
