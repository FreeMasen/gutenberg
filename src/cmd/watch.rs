use std::env;
use std::fs::{remove_dir_all};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::time::{Instant, Duration};

use chrono::prelude::*;
use notify::{Watcher, RecursiveMode, watcher};
use ctrlc;

use site::Site;
use errors::{Result, ResultExt};
use utils::fs::copy_file;

use console;
use rebuild;

#[derive(Debug, PartialEq)]
enum ChangeKind {
    Content,
    Templates,
    StaticFiles,
    Sass,
    Config,
}

fn rebuild_done_handling(broadcaster: &Option<Sender<String>>, res: Result<()>, reload_path: &str) {
    match res {
        Ok(_) => {
            if let Some(tx) = broadcaster {
                tx.send(format!(r#"
                    {{
                        "command": "reload",
                        "path": "{}",
                        "originalPath": "",
                        "liveCSS": true,
                        "liveImg": true,
                        "protocol": ["http://livereload.com/protocols/official-7"]
                    }}"#, reload_path)
                ).unwrap();
            }
        },
        Err(e) => console::unravel_errors("Failed to build the site", &e)
    }
}

fn create_new_site(output_dir: &str, base_url: &str, config_file: &str) -> Result<Site> {
    let mut site = Site::new(env::current_dir().unwrap(), config_file)?;
    site.set_base_url(base_url.to_string());
    site.set_output_path(output_dir);
    site.load()?;
    console::notify_site_size(&site);
    console::warn_about_ignored_pages(&site);
    site.build()?;
    Ok(site)
}

pub fn watch(output_dir: &str, base_url: &str, config_file: &str, broadcaster: &Option<Sender<String>>) -> Result<()> {
    let start = Instant::now();
    let mut site = create_new_site(output_dir, base_url, config_file)?;
    if let Some(ref broadcaster) = broadcaster {
        broadcaster.send(format!("first build complete")).unwrap();
    }
    console::report_elapsed_time(start);

    // Setup watchers
    let mut watching_static = false;
    let (tx, rx) = channel();
    let mut watcher = watcher(tx, Duration::from_secs(2)).unwrap();
    watcher.watch("content/", RecursiveMode::Recursive)
        .chain_err(|| "Can't watch the `content` folder. Does it exist?")?;
    watcher.watch("templates/", RecursiveMode::Recursive)
        .chain_err(|| "Can't watch the `templates` folder. Does it exist?")?;
    watcher.watch(config_file, RecursiveMode::Recursive)
        .chain_err(|| "Can't watch the `config` file. Does it exist?")?;

    if Path::new("static").exists() {
        watching_static = true;
        watcher.watch("static/", RecursiveMode::Recursive)
            .chain_err(|| "Can't watch the `static` folder. Does it exist?")?;
    }

    // Sass support is optional so don't make it an error to no have a sass folder
    let _ = watcher.watch("sass/", RecursiveMode::Recursive);

    let output_path = Path::new(output_dir).to_path_buf();

    let pwd = env::current_dir().unwrap();

    let mut watchers = vec!["content", "templates", "config.toml"];
    if watching_static {
        watchers.push("static");
    }
    if site.config.compile_sass {
        watchers.push("sass");
    }

    println!("Listening for changes in {}/{{{}}}", pwd.display(), watchers.join(", "));

    println!("Press Ctrl+C to stop\n");
    // Delete the output folder on ctrl+C
    ctrlc::set_handler(move || {
        remove_dir_all(&output_path).expect("Failed to delete output directory");
        ::std::process::exit(0);
    }).expect("Error setting Ctrl-C handler");

    use notify::DebouncedEvent::*;

    loop {
        match rx.recv() {
            Ok(event) => {
                match event {
                    Create(path) |
                    Write(path) |
                    Remove(path) |
                    Rename(_, path) => {
                        if is_temp_file(&path) || path.is_dir() {
                            continue;
                        }

                        println!("Change detected @ {}", Local::now().format("%Y-%m-%d %H:%M:%S").to_string());
                        let start = Instant::now();
                        match detect_change_kind(&pwd, &path) {
                            (ChangeKind::Content, _) => {
                                console::info(&format!("-> Content changed {}", path.display()));
                                // Force refresh
                                rebuild_done_handling(&broadcaster, rebuild::after_content_change(&mut site, &path), "/x.js");
                            },
                            (ChangeKind::Templates, _) => {
                                console::info(&format!("-> Template changed {}", path.display()));
                                // Force refresh
                                rebuild_done_handling(&broadcaster, rebuild::after_template_change(&mut site, &path), "/x.js");
                            },
                            (ChangeKind::StaticFiles, p) => {
                                if path.is_file() {
                                    console::info(&format!("-> Static file changes detected {}", path.display()));
                                    rebuild_done_handling(&broadcaster, copy_file(&path, &site.output_path, &site.static_path), &p.to_string_lossy());
                                }
                            },
                            (ChangeKind::Sass, p) => {
                                console::info(&format!("-> Sass file changed {}", path.display()));
                                rebuild_done_handling(&broadcaster, site.compile_sass(&site.base_path), &p.to_string_lossy());
                            },
                            (ChangeKind::Config, _) => {
                                console::info(&format!("-> Config changed. The whole site will be reloaded. The browser needs to be refreshed to make the changes visible."));
                                site = create_new_site(output_dir, base_url, config_file).unwrap();
                            }
                        };
                        console::report_elapsed_time(start);
                    }
                    _ => {}
                }
            },
            Err(e) => console::error(&format!("Watch error: {:?}", e)),
        };
    }
}

/// Returns whether the path we received corresponds to a temp file created
/// by an editor or the OS
fn is_temp_file(path: &Path) -> bool {
    let ext = path.extension();
    match ext {
        Some(ex) => match ex.to_str().unwrap() {
            "swp" | "swx" | "tmp" | ".DS_STORE" => true,
            // jetbrains IDE
            x if x.ends_with("jb_old___") => true,
            x if x.ends_with("jb_tmp___") => true,
            x if x.ends_with("jb_bak___") => true,
            // vim
            x if x.ends_with('~') => true,
            _ => {
                if let Some(filename) = path.file_stem() {
                    // emacs
                    filename.to_str().unwrap().starts_with('#')
                } else {
                    false
                }
            }
        },
        None => {
            true
        },
    }
}

/// Detect what changed from the given path so we have an idea what needs
/// to be reloaded
fn detect_change_kind(pwd: &Path, path: &Path) -> (ChangeKind, PathBuf) {
    let mut partial_path = PathBuf::from("/");
    partial_path.push(path.strip_prefix(pwd).unwrap_or(path));

    let change_kind = if partial_path.starts_with("/templates") {
        ChangeKind::Templates
    } else if partial_path.starts_with("/content") {
        ChangeKind::Content
    } else if partial_path.starts_with("/static") {
        ChangeKind::StaticFiles
    } else if partial_path.starts_with("/sass") {
        ChangeKind::Sass
    } else if partial_path == Path::new("/config.toml") {
        ChangeKind::Config
    } else {
        unreachable!("Got a change in an unexpected path: {}", partial_path.display());
    };

    (change_kind, partial_path)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{is_temp_file, detect_change_kind, ChangeKind};

    #[test]
    fn can_recognize_temp_files() {
        let test_cases = vec![
            Path::new("hello.swp"),
            Path::new("hello.swx"),
            Path::new(".DS_STORE"),
            Path::new("hello.tmp"),
            Path::new("hello.html.__jb_old___"),
            Path::new("hello.html.__jb_tmp___"),
            Path::new("hello.html.__jb_bak___"),
            Path::new("hello.html~"),
            Path::new("#hello.html"),
        ];

        for t in test_cases {
            assert!(is_temp_file(&t));
        }
    }

    #[test]
    fn can_detect_kind_of_changes() {
        let test_cases = vec![
            (
                (ChangeKind::Templates, PathBuf::from("/templates/hello.html")),
                Path::new("/home/vincent/site"), Path::new("/home/vincent/site/templates/hello.html")
            ),
            (
                (ChangeKind::StaticFiles, PathBuf::from("/static/site.css")),
                Path::new("/home/vincent/site"), Path::new("/home/vincent/site/static/site.css")
            ),
            (
                (ChangeKind::Content, PathBuf::from("/content/posts/hello.md")),
                Path::new("/home/vincent/site"), Path::new("/home/vincent/site/content/posts/hello.md")
            ),
            (
                (ChangeKind::Sass, PathBuf::from("/sass/print.scss")),
                Path::new("/home/vincent/site"), Path::new("/home/vincent/site/sass/print.scss")
            ),
            (
                (ChangeKind::Config, PathBuf::from("/config.toml")),
                Path::new("/home/vincent/site"), Path::new("/home/vincent/site/config.toml")
            ),
        ];

        for (expected, pwd, path) in test_cases {
            assert_eq!(expected, detect_change_kind(&pwd, &path));
        }
    }

    #[test]
    #[cfg(windows)]
    fn windows_path_handling() {
        let expected = (ChangeKind::Templates, PathBuf::from("/templates/hello.html"));
        let pwd = Path::new(r#"C:\\Users\johan\site"#);
        let path = Path::new(r#"C:\\Users\johan\site\templates\hello.html"#);
        assert_eq!(expected, detect_change_kind(pwd, path));
    }

    #[test]
    fn relative_path() {
        let expected = (ChangeKind::Templates, PathBuf::from("/templates/hello.html"));
        let pwd = Path::new("/home/johan/site");
        let path = Path::new("templates/hello.html");
        assert_eq!(expected, detect_change_kind(pwd, path));
    }
}
