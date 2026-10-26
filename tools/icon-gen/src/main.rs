use image::{ExtendedColorType, codecs::ico::{IcoEncoder, IcoFrame}};
use resvg::{tiny_skia, usvg};
use std::{env, error::Error, fs, path::Path};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 5 {
        eprintln!("usage: icon-gen <source.svg> <output.ico> <png-dir> <png-prefix>");
        std::process::exit(2);
    }
    let source = fs::read(&args[1])?;
    let tree = usvg::Tree::from_data(&source, &usvg::Options::default())?;
    let small_tree = env::var_os("ICON_SMALL_SVG")
        .map(fs::read)
        .transpose()?
        .map(|source| usvg::Tree::from_data(&source, &usvg::Options::default()))
        .transpose()?;
    let output_dir = Path::new(&args[3]);
    fs::create_dir_all(output_dir)?;
    let prefix = &args[4];
    let sizes = [16_u32, 20, 24, 32, 40, 48, 64, 128, 256];
    let mut frames = Vec::with_capacity(sizes.len());
    for size in sizes {
        let tree = if size <= 24 {
            small_tree.as_ref().unwrap_or(&tree)
        } else {
            &tree
        };
        let base_width = tree.size().width();
        let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("valid icon dimensions");
        let scale = size as f32 / base_width;
        resvg::render(
            tree,
            tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        let png = pixmap.encode_png()?;
        fs::write(output_dir.join(format!("{prefix}-{size}.png")), &png)?;
        frames.push(IcoFrame::with_encoded(
            png,
            size,
            size,
            ExtendedColorType::Rgba8,
        )?);
    }
    let output = fs::File::create(&args[2])?;
    IcoEncoder::new(output).encode_images(&frames)?;
    println!("generated {} and {} sizes", args[2], sizes.len());
    Ok(())
}
