use std::{
    collections::{HashMap, HashSet},
    path::{Component, Path, PathBuf},
};

pub(crate) struct RasterIcon {
    pub(crate) width: u32,
    pub(crate) rgba: Vec<u8>,
}

struct IconDirectory {
    path: PathBuf,
    size: u32,
    minimum: u32,
    maximum: u32,
    scale: u32,
    threshold: u32,
    kind: String,
}

pub(crate) fn load(name: &str, size: u32) -> Option<RasterIcon> {
    if name.is_empty() || name.contains('/') || name.contains("..") {
        return None;
    }
    let roots = icon_roots();
    let theme = configured_theme();
    let mut visited = HashSet::new();
    load_from_roots(name, size, &theme, &roots, &mut visited).or_else(|| {
        load_from_roots(name, size, "hicolor", &roots, &mut visited)
    })
}

fn icon_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join(".icons"));
    }
    let data_home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
    let data_home = data_home.or_else(|| {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(".local/share"))
    });
    if let Some(data_home) = data_home {
        roots.push(data_home.join("icons"));
    }
    let data_dirs = std::env::var_os("XDG_DATA_DIRS")
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    roots.extend(
        std::env::split_paths(&data_dirs).map(|path| path.join("icons")),
    );
    roots
}

fn configured_theme() -> String {
    if let Some(theme) = std::env::var_os("ICON_THEME")
        && !theme.is_empty()
    {
        return theme.to_string_lossy().into_owned();
    }
    let Some(home) = std::env::var_os("HOME") else {
        return "hicolor".into();
    };
    for version in ["gtk-4.0", "gtk-3.0"] {
        let path = PathBuf::from(&home)
            .join(".config")
            .join(version)
            .join("settings.ini");
        if let Ok(settings) = std::fs::read_to_string(path) {
            for line in settings.lines() {
                if let Some(value) =
                    line.trim().strip_prefix("gtk-icon-theme-name=")
                {
                    let value = value.trim();
                    if !value.is_empty() {
                        return value.into();
                    }
                }
            }
        }
    }
    "hicolor".into()
}

fn load_from_roots(
    name: &str,
    size: u32,
    theme: &str,
    roots: &[PathBuf],
    visited: &mut HashSet<String>,
) -> Option<RasterIcon> {
    if !visited.insert(theme.into()) {
        return None;
    }
    let mut candidates = Vec::new();
    let mut parents = Vec::new();
    for root in roots {
        let (inherited, directories) = read_theme(&root.join(theme));
        parents.extend(inherited);
        candidates.extend(directories.into_iter().flat_map(|directory| {
            let score = directory_score(&directory, size);
            ["png", "svg"].into_iter().filter_map(move |extension| {
                let path = directory.path.join(format!("{name}.{extension}"));
                score.map(|score| (score, path))
            })
        }));
    }
    candidates.sort_by_key(|(score, _)| *score);
    for (_, path) in candidates {
        if let Some(icon) = decode(&path, size) {
            return Some(icon);
        }
    }
    for parent in parents {
        if let Some(icon) = load_from_roots(name, size, &parent, roots, visited)
        {
            return Some(icon);
        }
    }
    None
}

fn read_theme(base: &Path) -> (Vec<String>, Vec<IconDirectory>) {
    let Ok(index) = std::fs::read_to_string(base.join("index.theme")) else {
        return (Vec::new(), Vec::new());
    };
    let mut parents = Vec::new();
    let mut directories = Vec::new();
    let mut section = String::new();
    let mut values = HashMap::new();
    let mut finish = |section: &str, values: &mut HashMap<String, String>| {
        if section == "Icon Theme" {
            if let Some(inherits) = values.get("Inherits") {
                parents.extend(
                    inherits
                        .split(',')
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned),
                );
            }
            return;
        }
        let path = Path::new(section);
        if section.is_empty()
            || path.is_absolute()
            || !path
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
        {
            return;
        }
        let Some(size) = values.get("Size").and_then(|size| size.parse().ok())
        else {
            return;
        };
        let minimum = values
            .get("MinSize")
            .and_then(|size| size.parse().ok())
            .unwrap_or(size);
        let maximum = values
            .get("MaxSize")
            .and_then(|size| size.parse().ok())
            .unwrap_or(size);
        directories.push(IconDirectory {
            path: base.join(path),
            size,
            minimum,
            maximum,
            scale: values
                .get("Scale")
                .and_then(|scale| scale.parse().ok())
                .unwrap_or(1),
            threshold: values
                .get("Threshold")
                .and_then(|value| value.parse().ok())
                .unwrap_or(2),
            kind: values
                .get("Type")
                .cloned()
                .unwrap_or_else(|| "Threshold".into()),
        });
    };
    for line in index.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            finish(&section, &mut values);
            section = line[1..line.len() - 1].trim().into();
            values.clear();
        } else if let Some((key, value)) = line.split_once('=') {
            values.insert(key.trim().into(), value.trim().into());
        }
    }
    finish(&section, &mut values);
    (parents, directories)
}

fn directory_score(directory: &IconDirectory, requested: u32) -> Option<u64> {
    let distance = match directory.kind.as_str() {
        "Fixed" => directory.size.abs_diff(requested),
        "Scalable" if requested < directory.minimum => {
            directory.minimum - requested
        }
        "Scalable" if requested > directory.maximum => {
            requested - directory.maximum
        }
        "Scalable" => 0,
        _ => directory.size.abs_diff(requested),
    };
    if directory.kind == "Threshold" && distance > directory.threshold {
        return None;
    }
    let distance = u64::from(distance);
    let scale = u64::from(directory.scale.abs_diff(1));
    Some(distance * 1000 + scale)
}

fn decode(path: &Path, size: u32) -> Option<RasterIcon> {
    if path.extension().is_some_and(|extension| extension == "svg") {
        return decode_svg(path, size.max(1));
    }
    let image = image::load_from_memory(&std::fs::read(path).ok()?).ok()?;
    let image = image.to_rgba8();
    if image.width() == 0 || image.width() != image.height() {
        return None;
    }
    Some(RasterIcon {
        width: image.width(),
        rgba: image.into_raw(),
    })
}

fn decode_svg(path: &Path, size: u32) -> Option<RasterIcon> {
    use resvg::usvg::{TreeParsing, TreeTextToPath};

    let data = std::fs::read(path).ok()?;
    let options = resvg::usvg::Options {
        resources_dir: std::fs::canonicalize(path)
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf)),
        ..Default::default()
    };
    let mut tree = resvg::usvg::Tree::from_data(&data, &options).ok()?;
    let mut fonts = resvg::usvg::fontdb::Database::new();
    fonts.load_system_fonts();
    tree.convert_text(&fonts);
    let tree = resvg::Tree::from_usvg(&tree);
    let width = tree.size.width();
    let height = tree.size.height();
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    let scale = size as f32 / width.max(height);
    let scaled_width = width * scale;
    let scaled_height = height * scale;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size)?;
    let transform = resvg::tiny_skia::Transform::from_scale(scale, scale)
        .post_translate(
            (size as f32 - scaled_width) / 2.0,
            (size as f32 - scaled_height) / 2.0,
        );
    tree.render(transform, &mut pixmap.as_mut());
    Some(RasterIcon {
        width: size,
        rgba: pixmap.data().to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "tilefire-icon-theme-{}-{id}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            Self { root }
        }

        fn add_icon(&self, theme: &str, directory: &str, size: &str) {
            let base = self.root.join(theme);
            std::fs::create_dir_all(base.join(directory)).unwrap();
            std::fs::write(
                base.join("index.theme"),
                format!(
                    "[Icon Theme]\nInherits=fallback\n[{directory}]\nSize={size}\nType=Fixed\n"
                ),
            )
            .unwrap();
            image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]))
                .save(base.join(directory).join("document-save.png"))
                .unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn loads_a_png_from_an_xdg_theme() {
        let fixture = Fixture::new();
        fixture.add_icon("example", "64x64/actions", "64");
        let icon = super::load_from_roots(
            "document-save",
            64,
            "example",
            std::slice::from_ref(&fixture.root),
            &mut Default::default(),
        )
        .unwrap();
        assert_eq!(icon.width, 2);
        assert_eq!(icon.rgba, [10, 20, 30, 255].repeat(4));
    }

    #[test]
    fn follows_theme_inheritance() {
        let fixture = Fixture::new();
        fixture.add_icon("parent", "64x64/actions", "64");
        let child = fixture.root.join("child");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(
            child.join("index.theme"),
            "[Icon Theme]\nInherits=parent\n",
        )
        .unwrap();
        let icon = super::load_from_roots(
            "document-save",
            64,
            "child",
            std::slice::from_ref(&fixture.root),
            &mut Default::default(),
        )
        .unwrap();
        assert_eq!(icon.width, 2);
    }

    #[test]
    fn rasterizes_svg_icons_at_the_requested_size() {
        let fixture = Fixture::new();
        let base = fixture.root.join("vector");
        let directory = base.join("64x64/actions");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            base.join("index.theme"),
            "[Icon Theme]\n[64x64/actions]\nSize=64\nType=Fixed\n",
        )
        .unwrap();
        std::fs::write(
            directory.join("document-save.svg"),
            "<svg xmlns='http://www.w3.org/2000/svg' width='24' height='24'><rect width='24' height='24' fill='#ff0000'/></svg>",
        )
        .unwrap();
        let icon = super::load_from_roots(
            "document-save",
            64,
            "vector",
            std::slice::from_ref(&fixture.root),
            &mut Default::default(),
        )
        .unwrap();
        assert_eq!(icon.width, 64);
        let center = (32 * 64 + 32) * 4;
        assert_eq!(icon.rgba[center..center + 4], [255, 0, 0, 255]);
    }
}
