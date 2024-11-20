#[allow(unused_imports)]
use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

use image::ImageOutputFormat;
use std::io::Cursor;
use std::net::TcpListener;
use std::time::SystemTime;
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};

#[derive(Debug)]
struct ImageOptions {
    width: Option<u32>,
    height: Option<u32>,
    quality: u8,
    format: ImageOutputFormat,
}

struct ImageCache {
    cache_dir: PathBuf,
    max_age: std::time::Duration,
}

impl ImageCache {
    fn new(cache_dir: PathBuf, max_age_days: u64) -> std::io::Result<Self> {
        fs::create_dir_all(&cache_dir)?;
        Ok(Self {
            cache_dir,
            max_age: std::time::Duration::from_secs(60 * 60 * 24 * max_age_days),
        })
    }

    fn get_cached(&self, key: &str) -> Option<Vec<u8>> {
        let path = self.cache_dir.join(key);

        if let Ok(metadata) = fs::metadata(&path) {
            if let Ok(modified) = metadata.modified() {
                if let Ok(age) = SystemTime::now().duration_since(modified) {
                    if age <= self.max_age {
                        return fs::read(path).ok();
                    }
                }
            }
        }
        None
    }

    fn store(&self, key: &str, data: &[u8]) -> std::io::Result<()> {
        let path = self.cache_dir.join(key);
        fs::write(path, data)
    }
}

impl ImageOptions {
    fn from_query(query: &str) -> Self {
        let params: Vec<(String, String)> = query
            .split('&')
            .filter_map(|param| {
                let mut parts = param.split('=');
                Some((parts.next()?.to_string(), parts.next()?.to_string()))
            })
            .collect();

        let mut opts = ImageOptions {
            width: None,
            height: None,
            quality: 80,                         // default quality
            format: ImageOutputFormat::Jpeg(80), // default format
        };

        for (key, value) in params {
            match key.as_str() {
                "w" | "width" => opts.width = value.parse().ok(),
                "h" | "height" => opts.height = value.parse().ok(),
                "q" | "quality" => {
                    if let Ok(q) = value.parse::<u8>() {
                        opts.quality = q.min(100);

                        if let ImageOutputFormat::Jpeg(_) = opts.format {
                            opts.format = ImageOutputFormat::Jpeg(opts.quality);
                        }
                    }
                }
                "fmt" => match value.as_str() {
                    "jpg" | "jpeg" => opts.format = ImageOutputFormat::Jpeg(opts.quality),
                    "png" => opts.format = ImageOutputFormat::Png,
                    "webp" => opts.format = ImageOutputFormat::WebP,
                    _ => {}
                },
                _ => {}
            }
        }
        opts
    }
    fn cache_key(&self, original_path: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(original_path.as_bytes());
        hasher.update(
            format!(
                "w{:?}h{:?}q{}fmt{:?}",
                self.width, self.height, self.quality, self.format
            )
            .as_bytes(),
        );

        format!("{:x}", hasher.finalize())
    }
}

fn optimize_image(img_data: &[u8], options: &ImageOptions) -> Result<Vec<u8>, image::ImageError> {
    let img = image::load_from_memory(img_data)?;

    let resized = match (options.width, options.height) {
        (Some(w), Some(h)) => img.resize(w, h, image::imageops::FilterType::Lanczos3),
        (Some(w), None) => {
            let ratio = w as f32 / img.width() as f32;
            let new_height = (img.height() as f32 * ratio) as u32;
            img.resize(w, new_height, image::imageops::FilterType::Lanczos3)
        }
        (None, Some(h)) => {
            let ratio = h as f32 / img.height() as f32;
            let new_width = (img.width() as f32 * ratio) as u32;
            img.resize(new_width, h, image::imageops::FilterType::Lanczos3)
        }
        (None, None) => img,
    };

    let mut buffer = Vec::new();
    let mut cursor = Cursor::new(&mut buffer);
    resized.write_to(&mut cursor, options.format.clone())?;

    Ok(buffer)
}

fn main() {
    let listener = TcpListener::bind("127.0.0.1:4221").unwrap();

    for stream in listener.incoming() {
        match stream {
            Ok(mut _stream) => {
                let mut buffer = [0; 1024];
                let bytes_read = _stream.read(&mut buffer).unwrap();

                let request = String::from_utf8_lossy(&buffer[..bytes_read]);
                let response = handle_request(&request);

                _stream.write_all(&response).unwrap();
                _stream.flush().unwrap();
            }
            Err(e) => {
                println!("error: {}", e);
            }
        }
    }
}

fn status_text(code: u32) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Internal Server Error",
    }
}

fn handle_request(request: &str) -> Vec<u8> {
    let first_line = request.lines().next().unwrap();
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap();
    let full_path = parts.next().unwrap();
    let _version = parts.next().unwrap();

    let mut path_parts = full_path.split('?');
    let path = path_parts.next().unwrap();
    let query = path_parts.next();

    println!("{} {} {}", method, path, query.unwrap_or(""));

    let accepts_gzip = request.lines().any(|line| {
        line.to_lowercase().starts_with("accept-encoding:") && line.to_lowercase().contains("gzip")
    });
    print!("Accepts gzip: {}\n", accepts_gzip);

    match (method, path) {
        ("GET", "/") => serve_file("/index.html", accepts_gzip, None),
        ("GET", path) => serve_file(path, accepts_gzip, query),
        _ => build_response(404, Some("Not found")),
    }
}

fn serve_file(path: &str, accepts_gzip: bool, query: Option<&str>) -> Vec<u8> {
    let file_path = Path::new("public").join(path.trim_start_matches('/'));

    if !file_path.starts_with("public") {
        return build_response(403, Some("Forbidden"));
    }

    match fs::read(&file_path) {
        Ok(content) => {
            let content_type = get_content_type(&file_path);

            if is_image_content_type(content_type) && query.is_some() {
                let options = ImageOptions::from_query(query.unwrap());

                let cache = ImageCache::new(PathBuf::from("cache/images"), 7)
                    .unwrap_or_else(|_| panic!("Failed to create cache directory"));

                let cache_key = options.cache_key(path);

                if let Some(cached_image) = cache.get_cached(&cache_key) {
                    println!("Cache hit for {}", path);
                    let new_content_type = match options.format {
                        ImageOutputFormat::Jpeg(_) => "image/jpeg",
                        ImageOutputFormat::Png => "image/png",
                        ImageOutputFormat::WebP => "image/webp",
                        _ => content_type,
                    };
                    return build_response_with_type(200, Some(&cached_image), new_content_type);
                }

                println!("Cache miss for {}", path);
                match optimize_image(&content, &options) {
                    Ok(optimized) => {
                        if let Err(e) = cache.store(&cache_key, &optimized) {
                            eprintln!("Failed to cache image: {}", e);
                        }

                        let new_content_type = match options.format {
                            ImageOutputFormat::Jpeg(_) => "image/jpeg",
                            ImageOutputFormat::Png => "image/png",
                            ImageOutputFormat::WebP => "image/webp",
                            _ => content_type,
                        };
                        return build_response_with_type(200, Some(&optimized), new_content_type);
                    }
                    Err(_) => return build_response(500, Some("Image processing failed")),
                }
            }

            let should_compress =
                accepts_gzip && is_compressible(content_type) && content.len() > 1024;
            if should_compress {
                build_compressed_response(200, &content, content_type)
            } else {
                build_response_with_type(200, Some(&content), content_type)
            }
        }
        Err(_) => build_response(404, Some("Not found")),
    }
}

fn is_compressible(content_type: &str) -> bool {
    match content_type {
        "text/html"
        | "text/css"
        | "application/javascript"
        | "text/javascript"
        | "text/plain"
        | "application/json"
        | "application/xml" => true,
        _ => false,
    }
}

fn get_content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html",
        Some("css") => "text/css",
        Some("js") => "application/javascript",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        _ => "application/octet-stream",
    }
}

fn build_compressed_response(status_code: u32, content: &[u8], content_type: &str) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write(content).unwrap();
    let compressed = encoder.finish().unwrap();

    println!("Original size: {}", content.len());
    println!("Compressed size: {}", compressed.len());
    let status = format!("{} {}", status_code, status_text(status_code));
    let headers = format!(
        "HTTP/1.1 {}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Content-Encoding: gzip\r\n\
         Vary: Accept-Encoding\r\n\
         \r\n",
        status,
        content_type,
        compressed.len()
    );

    let mut response = headers.into_bytes();
    response.extend_from_slice(&compressed);
    response
}

fn build_response(status_code: u32, body: Option<&str>) -> Vec<u8> {
    let body_content = body.unwrap_or("");
    let status = format!("{} {}", status_code, status_text(status_code));
    let response = format!(
        "HTTP/1.1 {}\r\nContent-Length: {}\r\n\r\n{}",
        status,
        body_content.len(),
        body_content
    );
    response.into_bytes()
}

fn build_response_with_type(status_code: u32, body: Option<&[u8]>, content_type: &str) -> Vec<u8> {
    let body_content = body.unwrap_or(&[]);
    let status = format!("{} {}", status_code, status_text(status_code));
    let headers = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\r\n",
        status,
        content_type,
        body_content.len()
    );

    let mut response = headers.into_bytes();
    response.extend_from_slice(body_content);
    response
}

fn is_image_content_type(content_type: &str) -> bool {
    matches!(
        content_type,
        "image/jpeg" | "image/png" | "image/webp" | "image/gif"
    )
}
