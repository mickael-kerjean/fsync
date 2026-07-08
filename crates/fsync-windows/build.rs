use std::io::Write;
use std::path::Path;

const SIZES: [u32; 4] = [16, 20, 24, 32];

const ICONS: [(&str, &str); 4] = [
    ("icon-base.svg", "tray-unlogged.ico"),
    ("icon-ok.svg", "tray-ok.ico"),
    ("icon-sync.svg", "tray-sync.ico"),
    ("icon-error.svg", "tray-error.ico"),
];

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let svg_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fsync-core/icons");
    for (svg, ico) in ICONS {
        let svg_path = svg_dir.join(svg);
        println!("cargo:rerun-if-changed={}", svg_path.display());
        let frames: Vec<(u32, Vec<u8>)> = SIZES
            .iter()
            .map(|&size| (size, rasterize(&svg_path, size)))
            .collect();
        write_ico(Path::new(&out_dir).join(ico), &frames);
    }

    let base = svg_dir.join("icon-base.svg");
    let frames: Vec<(u32, Vec<u8>)> = [16, 24, 32, 48, 256]
        .iter()
        .map(|&size| (size, rasterize(&base, size)))
        .collect();
    write_ico(Path::new(&out_dir).join("app.ico"), &frames);
    std::fs::write(Path::new(&out_dir).join("app.rc"), "1 ICON \"app.ico\"\n")
        .expect("write app.rc");
    embed_resource::compile(Path::new(&out_dir).join("app.rc"), embed_resource::NONE)
        .manifest_optional()
        .expect("compile app.rc");
}

fn rasterize(svg: &Path, size: u32) -> Vec<u8> {
    let data = std::fs::read(svg).unwrap_or_else(|err| panic!("read {}: {err}", svg.display()));
    let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default())
        .unwrap_or_else(|err| panic!("parse {}: {err}", svg.display()));
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).expect("pixmap");
    let scale = size as f32 / tree.size().width().max(tree.size().height());
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap
        .pixels()
        .iter()
        .flat_map(|px| {
            let px = px.demultiply();
            [px.red(), px.green(), px.blue(), px.alpha()]
        })
        .collect()
}

fn write_ico(path: std::path::PathBuf, frames: &[(u32, Vec<u8>)]) {
    let mut file = std::io::BufWriter::new(std::fs::File::create(&path).expect("create ico"));
    let header_len = 6 + 16 * frames.len() as u32;
    file.write_all(&[0, 0, 1, 0]).unwrap();
    file.write_all(&(frames.len() as u16).to_le_bytes())
        .unwrap();
    let mut offset = header_len;
    for (size, _) in frames {
        let bytes = frame_len(*size);
        file.write_all(&[*size as u8, *size as u8, 0, 0]).unwrap();
        file.write_all(&1u16.to_le_bytes()).unwrap();
        file.write_all(&32u16.to_le_bytes()).unwrap();
        file.write_all(&bytes.to_le_bytes()).unwrap();
        file.write_all(&offset.to_le_bytes()).unwrap();
        offset += bytes;
    }
    for (size, rgba) in frames {
        let (size, mask_stride) = (*size, mask_stride(*size));
        file.write_all(&40u32.to_le_bytes()).unwrap();
        file.write_all(&(size as i32).to_le_bytes()).unwrap();
        file.write_all(&(2 * size as i32).to_le_bytes()).unwrap();
        file.write_all(&1u16.to_le_bytes()).unwrap();
        file.write_all(&32u16.to_le_bytes()).unwrap();
        file.write_all(&[0u8; 24]).unwrap();
        for row in (0..size).rev() {
            for col in 0..size {
                let px = ((row * size + col) * 4) as usize;
                file.write_all(&[rgba[px + 2], rgba[px + 1], rgba[px], rgba[px + 3]])
                    .unwrap();
            }
        }
        file.write_all(&vec![0u8; (mask_stride * size) as usize])
            .unwrap();
    }
}

fn mask_stride(size: u32) -> u32 {
    size.div_ceil(32) * 4
}

fn frame_len(size: u32) -> u32 {
    40 + size * size * 4 + mask_stride(size) * size
}
