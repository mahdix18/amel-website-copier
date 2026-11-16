//! Packing a finished copy into a zip, and serving a copy over local HTTP.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use mc_core::STATE_DIR;
use zip::write::SimpleFileOptions;

/// Zips everything in `output` except crawl state and earlier zips, into
/// `output/<host>.zip` when the copy holds one site, else `output/copy.zip`.
pub fn zip_copy(output: &Path) -> io::Result<PathBuf> {
    let name = match site_dirs(output)?.as_slice() {
        [single] => format!("{}.zip", file_name(single)),
        _ => "copy.zip".to_owned(),
    };
    let target = output.join(name);
    let mut zip = zip::ZipWriter::new(File::create(&target)?);
    let options = SimpleFileOptions::default();
    let mut files = Vec::new();
    walk(output, &mut files)?;
    files.sort();
    for file in files {
        let relative = file.strip_prefix(output).map_err(io::Error::other)?;
        let entry = relative.to_string_lossy().replace('\\', "/");
        let top = entry.split('/').next().unwrap_or_default();
        if top == STATE_DIR || (relative.parent() == Some(Path::new("")) && entry.ends_with(".zip"))
        {
            continue;
        }
        zip.start_file(entry, options).map_err(io::Error::other)?;
        io::copy(&mut File::open(&file)?, &mut zip)?;
    }
    zip.finish().map_err(io::Error::other)?;
    Ok(target)
}

/// The directory to serve: the site folder when the copy holds just one,
/// so the site's own absolute paths (`/api/…`) resolve.
pub fn serve_root(output: &Path) -> io::Result<PathBuf> {
    Ok(match site_dirs(output)?.as_slice() {
        [single] => single.clone(),
        _ => output.to_path_buf(),
    })
}

/// The per-host folders of a copy.
fn site_dirs(output: &Path) -> io::Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(output)? {
        let path = entry?.path();
        if path.is_dir() && file_name(&path) != STATE_DIR {
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(dirs)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn walk(dir: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            walk(&path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zips_the_copy_without_state() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path();
        std::fs::create_dir_all(out.join("site.test/posts")).unwrap();
        std::fs::create_dir_all(out.join(".amel")).unwrap();
        std::fs::write(out.join("site.test/index.html"), "<p>home</p>").unwrap();
        std::fs::write(out.join("site.test/posts/a b.html"), "<p>a</p>").unwrap();
        std::fs::write(out.join("amel-report.html"), "report").unwrap();
        std::fs::write(out.join(".amel/state.db"), "secret state").unwrap();

        let zip_path = zip_copy(out).unwrap();
        assert_eq!(zip_path, out.join("site.test.zip"));
        // Zipping again replaces, and never includes, the earlier zip.
        let zip_path = zip_copy(out).unwrap();
        let mut archive = zip::ZipArchive::new(File::open(zip_path).unwrap()).unwrap();
        let mut names: Vec<String> = archive
            .file_names()
            .map(|name| name.unwrap().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "amel-report.html",
                "site.test/index.html",
                "site.test/posts/a b.html"
            ]
        );
        let mut page = String::new();
        io::Read::read_to_string(
            &mut archive.by_name("site.test/index.html").unwrap(),
            &mut page,
        )
        .unwrap();
        assert_eq!(page, "<p>home</p>");

        assert_eq!(serve_root(out).unwrap(), out.join("site.test"));
        std::fs::create_dir_all(out.join("other.test")).unwrap();
        assert_eq!(serve_root(out).unwrap(), out.to_path_buf());
    }
}
