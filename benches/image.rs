//! Exercise the binary's production code without adding a public library API.

#[allow(dead_code)]
#[macro_use]
#[path = "../src/profile.rs"]
mod profile;
// The custom harness omits image.rs's unit tests but still compiles their imports.
#[allow(dead_code, unused_imports)]
#[path = "../src/image.rs"]
mod image;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use image::Image;
use std::hint::black_box;

fn fixture(w: u32, h: u32, noise: bool, alpha: bool) -> Image {
    let mut img = Image::new(w, h);
    let mut state = 0x1234_5678u32;
    for (i, p) in img.data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let (x, y) = (i as u32 % w, i as u32 / w);
        // Flat panels and fine stripes resemble UI content; xorshift supplies
        // a reproducible high-entropy alternative without a random dependency.
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        let rgb = if noise {
            [state as u8, (state >> 8) as u8, (state >> 16) as u8]
        } else {
            let panel = ((x / 240 + y / 120) % 4) as u8 * 48;
            let stripe = if y % 24 < 2 && x % 160 < 120 { 24 } else { 0 };
            [panel + stripe, panel + 16, panel + 32]
        };
        p.copy_from_slice(&[rgb[0], rgb[1], rgb[2], if alpha { (x % 256) as u8 } else { 255 }]);
    }
    img
}

fn transforms(c: &mut Criterion) {
    let mut group = c.benchmark_group("image");
    for (w, h) in [(1920, 1080), (3840, 2160)] {
        let size = format!("{w}x{h}");
        let img = fixture(w, h, false, false);
        group.throughput(Throughput::Elements(u64::from(w) * u64::from(h)));

        // Model a padded BGRX capture buffer, not only tightly packed rows.
        let stride = w as usize * 4 + 64;
        let mut bgrx = vec![0; stride * h as usize];
        for (src, dst) in img.data.chunks_exact(w as usize * 4).zip(bgrx.chunks_exact_mut(stride)) {
            for (s, d) in src.as_chunks::<4>().0.iter().zip(dst.as_chunks_mut::<4>().0.iter_mut()) {
                d.copy_from_slice(&[s[2], s[1], s[0], 0]);
            }
        }
        group.bench_function(BenchmarkId::new("from_bgrx_padded", &size), |b| {
            b.iter(|| Image::from_rows(w, h, stride, 4, black_box(&bgrx), |p| [p[2], p[1], p[0], 255]).unwrap())
        });
        group.bench_function(BenchmarkId::new("crop_centre_half", &size), |b| b.iter(|| black_box(&img).crop(w / 4, h / 4, w / 2, h / 2)));
        group.bench_function(BenchmarkId::new("resize_half", &size), |b| b.iter(|| black_box(&img).resize(w / 2, h / 2)));
        group.bench_function(BenchmarkId::new("resize_three_quarters", &size), |b| b.iter(|| black_box(&img).resize(w * 3 / 4, h * 3 / 4)));
        group.bench_function(BenchmarkId::new("rotate_90", &size), |b| b.iter(|| black_box(&img).rotate(90)));
    }
    group.finish();
}

fn png(c: &mut Criterion) {
    let mut group = c.benchmark_group("png");
    for (w, h) in [(1920, 1080), (3840, 2160)] {
        group.throughput(Throughput::Elements(u64::from(w) * u64::from(h)));
        for (name, noise, alpha) in [("ui_rgb", false, false), ("noise_rgb", true, false), ("ui_rgba", false, true)] {
            let img = fixture(w, h, noise, alpha);
            let encoded = img.encode_png().unwrap();
            // Check the fixtures outside timing, including both encoder paths.
            assert_eq!(Image::decode_png(&encoded).unwrap().data, img.data);
            assert_eq!(Image::decode_png(&img.encode_png_quick().unwrap()).unwrap().data, img.data);
            let case = format!("{name}/{w}x{h}");
            group.bench_function(BenchmarkId::new("encode", &case), |b| b.iter(|| black_box(&img).encode_png().unwrap()));
            group.bench_function(BenchmarkId::new("encode_quick", &case), |b| b.iter(|| black_box(&img).encode_png_quick().unwrap()));
            group.bench_function(BenchmarkId::new("decode", &case), |b| b.iter(|| Image::decode_png(black_box(&encoded)).unwrap()));
        }
    }
    group.finish();
}

criterion_group!(benches, transforms, png);
criterion_main!(benches);
