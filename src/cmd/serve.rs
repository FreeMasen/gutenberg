// Contains an embedded version of livereload-js
//
// Copyright (c) 2010-2012 Andrey Tarantsov
//
// Permission is hereby granted, free of charge, to any person obtaining
// a copy of this software and associated documentation files (the
// "Software"), to deal in the Software without restriction, including
// without limitation the rights to use, copy, modify, merge, publish,
// distribute, sublicense, and/or sell copies of the Software, and to
// permit persons to whom the Software is furnished to do so, subject to
// the following conditions:
//
// The above copyright notice and this permission notice shall be
// included in all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
// EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
// NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE
// LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
// OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION
// WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

use std::fs::{File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::thread;

use actix_web::{self, fs, http, server, App, HttpRequest, HttpResponse, Responder};
use actix_web::middleware::{Middleware, Started, Response, Logger};
use ws::{WebSocket, Sender, Message};
use utils::net::get_available_port;

use errors::{Result};
use console;
use super::watch;


// Uglified using uglifyjs
// Also, commenting out the lines 330-340 (containing `e instanceof ProtocolError`) was needed
// as it seems their build didn't work well and didn't include ProtocolError so it would error on
// errors
const LIVE_RELOAD: &'static str = include_str!("livereload.js");

struct NotFoundHandler {
    rendered_template: PathBuf,
}

impl<S> Middleware<S> for NotFoundHandler {
    fn start(&self, _req: &HttpRequest<S>) -> actix_web::Result<Started> {
        Ok(Started::Done)
    }

    fn response(
        &self,
        _req: &HttpRequest<S>,
        mut resp: HttpResponse,
    ) -> actix_web::Result<Response> {
        if http::StatusCode::NOT_FOUND == resp.status() {
            let mut fh = File::open(&self.rendered_template)?;
            let mut buf: Vec<u8> = vec![];
            let _ = fh.read_to_end(&mut buf)?;
            resp.replace_body(buf);
            resp.headers_mut().insert(
                http::header::CONTENT_TYPE,
                http::header::HeaderValue::from_static("text/html"),
            );
        }
        Ok(Response::Done(resp))
    }
}

fn livereload_handler(_: &HttpRequest) -> &'static str {
    LIVE_RELOAD
}



/// Attempt to render `index.html` when a directory is requested.
///
/// The default "batteries included" mechanisms for actix to handle directory
/// listings rely on redirection which behaves oddly (the location headers
/// seem to use relative paths for some reason).
/// They also mean that the address in the browser will include the
/// `index.html` on a successful redirect (rare), which is unsightly.
///
/// Rather than deal with all of that, we can hijack a hook for presenting a
/// custom directory listing response and serve it up using their
/// `NamedFile` responder.
fn handle_directory<'a, 'b>(dir: &'a fs::Directory, req: &'b HttpRequest) -> io::Result<HttpResponse> {
    let mut path = PathBuf::from(&dir.base);
    path.push(&dir.path);
    path.push("index.html");
    fs::NamedFile::open(path)?.respond_to(req)
}

pub fn serve(interface: &str, port: &str, output_dir: &str, _base_url: &str, config_file: &str) -> Result<()> {
    println!("serve {}, {}, {}, {}, {}", interface, port, output_dir, _base_url, config_file);
    let (tx, rx) = channel();

    let ws_address = format!("{}:{}", interface, get_available_port().unwrap());
    println!("ws_address: {}", ws_address);
    let output_path = Path::new(output_dir).to_path_buf();
    let address = format!("{}:{}", interface, port);
    println!("address: {}", address);
    // output path is going to need to be moved later on, so clone it for the
    // http closure to avoid contention.
    let static_root = output_path.clone();
    let base_url = address.clone();
    let config_file = config_file.to_string();
    let output_dir = output_dir.to_string();
    thread::spawn(move || {
        watch::watch(&output_dir, &base_url, &config_file, &Some(tx)).unwrap();
    });
    //wait for the first build to complete
    rx.recv().unwrap();

    thread::spawn(move || {

        println!("starting server, static_root: {:?}", static_root);
        let s = server::new(move || {
            App::new()
            .middleware(Logger::default())
            .middleware(NotFoundHandler { rendered_template: static_root.join("404.html") })
            .resource(r"/livereload.js", |r| r.f(livereload_handler))
            // Start a webserver that serves the `output_dir` directory
            .handler(
                r"/",
                fs::StaticFiles::new(&static_root)
                    .unwrap()
                    .show_files_listing()
                    .files_listing_renderer(handle_directory)
            )
        })
        .bind(&address)
        .expect("Can't start the webserver")
        .shutdown_timeout(20);
        println!("Web server is available at http://{}", &address);
        s.run();
    });

    // The websocket for livereload
    let ws_server = WebSocket::new(|output: Sender| {
        move |msg: Message| {
            if msg.into_text().unwrap().contains("\"hello\"") {
                return output.send(Message::text(r#"
                    {
                        "command": "hello",
                        "protocols": [ "http://livereload.com/protocols/official-7" ],
                        "serverName": "Gutenberg"
                    }
                "#));
            }
            Ok(())
        }
    }).expect("Failed to create ws server");
    println!("starting ws server");
    let broadcaster = ws_server.broadcaster();
    thread::spawn(move || {
        ws_server.listen(&*ws_address).unwrap();
    });

    loop {
        match rx.recv() {
            Ok(msg) => {
                broadcaster.send(msg).unwrap();
            },
            Err(e) => console::error(&format!("Watch error: {:?}", e)),
        };
    }
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
