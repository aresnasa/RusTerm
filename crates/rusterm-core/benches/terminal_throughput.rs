//! Throughput benchmarks for the terminal core.
//!
//! These exist to lock in the performance characteristics of the hottest paths
//! — parsing output, scrolling into scrollback, evicting old scrollback lines,
//! and rendering — so that a future change cannot silently reintroduce an
//! O(n)-per-line regression (the original `Vec::remove(0)` scrollback eviction
//! made the terminal progressively slower as scrollback filled).
//!
//! Run with: `cargo bench -p rusterm-core`

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use rusterm_core::terminal::{Terminal, TerminalSize};
use vte::ansi::Processor;

/// Build a payload of `lines` plain-text rows, each `cols` wide, newline-ended.
fn make_plain_text(lines: usize, cols: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(lines * (cols + 2));
    let row: Vec<u8> = (0..cols).map(|i| b'a' + (i % 26) as u8).collect();
    for _ in 0..lines {
        buf.extend_from_slice(&row);
        buf.extend_from_slice(b"\r\n");
    }
    buf
}

/// Process `data` through a fresh terminal of the given size, returning the
/// terminal so the caller can inspect/render it.
fn process_all(size: TerminalSize, data: &[u8]) -> Terminal {
    let mut term = Terminal::new(size);
    let mut parser = Processor::new();
    // Feed in realistic chunks (a few KiB) — mirrors how the SSH/PTY reader
    // delivers data, and means process()/scan_exit_codes run per-chunk.
    for chunk in data.chunks(4096) {
        term.process(chunk, &mut parser);
    }
    term
}

fn bench_process_plain_text(c: &mut Criterion) {
    let size = TerminalSize { cols: 80, rows: 24, ..Default::default() };
    let mut group = c.benchmark_group("process_plain_text");
    for &lines in &[1_000usize, 5_000, 20_000] {
        let data = make_plain_text(lines, 80);
        group.throughput(Throughput::Bytes(data.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(lines), &data, |b, data| {
            b.iter(|| {
                let term = process_all(size, black_box(data));
                black_box(term);
            });
        });
    }
    group.finish();
}

/// The headline regression guard: once scrollback exceeds capacity, every new
/// scrolled line evicts the oldest. Before the VecDeque fix this eviction was
/// `Vec::remove(0)` = O(scrollback), so throughput collapsed as scrollback grew.
/// The 20_000-line case runs well past the 10_000 default capacity and must stay
/// roughly constant per-byte regardless of how full scrollback is.
fn bench_sustained_scrollback_eviction(c: &mut Criterion) {
    let size = TerminalSize { cols: 80, rows: 24, ..Default::default() };
    let mut group = c.benchmark_group("sustained_scrollback_eviction");
    for &lines in &[5_000usize, 10_000, 20_000, 50_000] {
        let data = make_plain_text(lines, 80);
        group.throughput(Throughput::Bytes(data.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(lines), &data, |b, data| {
            b.iter(|| {
                let term = process_all(size, black_box(data));
                // Capacity is 10_000 by default: for lines > 10_000 the terminal
                // is evicting on (almost) every line. Assert it never grows past
                // capacity to keep the benchmark honest.
                debug_assert!(term.scrollback_len() <= term.scrollback_capacity());
                black_box(term);
            });
        });
    }
    group.finish();
}

/// Output interleaved with FinalTerm shell-integration markers (OSC 133;D).
/// Exercises the scan_exit_codes hot path that runs on every process() call.
fn bench_process_with_exit_codes(c: &mut Criterion) {
    let size = TerminalSize { cols: 80, rows: 24, ..Default::default() };
    let mut data = Vec::with_capacity(20_000 * 90);
    let row: Vec<u8> = (0..80).map(|i| b'a' + (i % 26) as u8).collect();
    for n in 0..20_000usize {
        data.extend_from_slice(&row);
        data.extend_from_slice(b"\r\n");
        // Shell reports the exit code after each "command".
        let code = if n % 7 == 0 { 1 } else { 0 };
        data.extend_from_slice(format!("\x1b]133;D;{code}\x07").as_bytes());
    }

    let mut group = c.benchmark_group("process_with_exit_codes");
    group.throughput(Throughput::Bytes(data.len() as u64));
    group.bench_function("20k_lines_osc133", |b| {
        b.iter(|| {
            let term = process_all(size, black_box(&data));
            black_box(term);
        });
    });
    group.finish();
}

/// Render after the screen is full — the cost the UI pays per frame.
fn bench_render(c: &mut Criterion) {
    let size = TerminalSize { cols: 80, rows: 24, ..Default::default() };
    let data = make_plain_text(100, 80);
    let term = process_all(size, &data);

    let mut group = c.benchmark_group("render");
    group.bench_function("at_bottom", |b| {
        b.iter(|| black_box(term.render()));
    });
    group.bench_function("scrolled_mid", |b| {
        b.iter(|| black_box(term.render_with_scroll(50)));
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_process_plain_text,
    bench_sustained_scrollback_eviction,
    bench_process_with_exit_codes,
    bench_render,
);
criterion_main!(benches);
