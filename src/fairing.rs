use lazy_static::lazy_static;
use rocket::{
    fairing::{Fairing, Info, Kind},
    http::{hyper::header::{CONTENT_ENCODING, CACHE_CONTROL}, Header, MediaType},
    tokio::{
        io::{AsyncRead, ReadBuf},
        sync::RwLock,
    },
    Request, Response,
};
use std::{collections::HashMap, io::Cursor, task::Poll};

use crate::{CompressionUtils, Encoding};

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub(crate) enum CachedEncoding {
    Gzip,
    Brotli,
}

lazy_static! {
    static ref EXCLUSIONS: Vec<MediaType> = vec![
        MediaType::parse_flexible("application/gzip").unwrap(),
        MediaType::parse_flexible("application/zip").unwrap(),
        MediaType::parse_flexible("image/*").unwrap(),
        MediaType::parse_flexible("video/*").unwrap(),
        MediaType::parse_flexible("application/octet-stream").unwrap(),
    ];
    static ref CACHED_FILES: RwLock<HashMap<(String, CachedEncoding), &'static [u8]>> = {
        let m = HashMap::new();
        RwLock::new(m)
    };
}

/// Compresses all responses with Brotli or Gzip compression.
///
/// Compression is done in the same manner as the [`Compress`](super::Compress)
/// responder.
///
/// By default, the fairing does not compress responses with a `Content-Type`
/// matching any of the following:
///
/// - `application/gzip`
/// - `application/zip`
/// - `image/*`
/// - `video/*`
/// - `application/octet-stream`
///
/// # Usage
///
/// Attach the compression [fairing](/rocket/fairing/) to your Rocket
/// application:
///
/// ```rust
///
/// use rocket_async_compression::Compression;
///
///
/// rocket::build()
///     // ...
///     .attach(Compression::fairing())
///     // ...
///     # ;
///
/// ```
pub struct Compression(());

impl Compression {
    /// Returns a fairing that compresses outgoing requests.
    ///
    /// ## Example
    /// To attach this fairing, simply call `attach` on the application's
    /// `Rocket` instance with `Compression::fairing()`:
    ///
    /// ```rust
    ///
    /// use rocket_async_compression::Compression;
    ///
    /// rocket::build()
    ///     // ...
    ///     .attach(Compression::fairing())
    ///     // ...
    ///     # ;
    /// ```
    pub fn fairing() -> Compression {
        Compression(())
    }
}

#[rocket::async_trait]
impl Fairing for Compression {
    fn info(&self) -> Info {
        Info {
            name: "Response compression",
            kind: Kind::Response,
        }
    }

    async fn on_response<'r>(&self, request: &'r Request<'_>, response: &mut Response<'r>) {
        super::CompressionUtils::compress_response(request, response, &EXCLUSIONS);
    }
}

/// Compresses all responses with Brotli or Gzip compression. Caches compressed
/// response bodies in memory for selected file types/path suffixes, useful for
/// compressing large compiled JS/CSS files, OTF font packs, etc.  Note that all
/// cached files are held in memory indefinitely.
///
/// Compression is done in the same manner as the [`Compression`](Compression)
/// fairing.
///
/// # Usage
///
/// Attach the compression [fairing](/rocket/fairing/) to your Rocket
/// application:
///
/// ```rust
///
/// use rocket_async_compression::CachedCompression;
///
/// rocket::build()
///     // ...
///     .attach(CachedCompression::fairing(vec![".otf", "main.dart.js"]))
///     // ...
///     # ;
///
/// ```
pub struct CachedCompression {
    pub cached_path_endings: Vec<&'static str>,
}

impl CachedCompression {
    pub fn fairing(cached_path_endings: Vec<&'static str>) -> CachedCompression {
        CachedCompression {
            cached_path_endings,
        }
    }
}

/// When performing cached compression on a body, it is possible that reading the existing body will fail.  We can't return an error directly from a fairing, so we forward the
/// error on to the response by setting in this dummy body which just returns the error.
struct ErrorBody(Option<std::io::Error>);

impl AsyncRead for ErrorBody {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let err = match self.0.take() {
            Some(err) => err,
            None => std::io::Error::new(std::io::ErrorKind::Other, "ErrorBody already read"),
        };
        Poll::Ready(Err(err))
    }
}

#[rocket::async_trait]
impl Fairing for CachedCompression {
    fn info(&self) -> Info {
        Info {
            name: "Cached response compression",
            kind: Kind::Response,
        }
    }

    async fn on_response<'r>(&self, request: &'r Request<'_>, response: &mut Response<'r>) {
        let path = request.uri().path().to_string();
        let cache_compressed_responses = self.cached_path_endings.iter().any(|s| path.ends_with(s));
        if !cache_compressed_responses {
            return;
        }

        let (accepts_gzip, accepts_br) = CompressionUtils::accepted_algorithms(request);
        if !accepts_gzip && !accepts_br {
            return;
        }

        if CompressionUtils::already_encoded(response) {
            return;
        }

        let content_type = response.content_type();
        if CompressionUtils::skip_encoding(&content_type, &EXCLUSIONS) {
            return;
        }

        let desired_encoding = if accepts_br {
            CachedEncoding::Brotli
        } else {
            CachedEncoding::Gzip
        };
        let encoding = match desired_encoding {
            CachedEncoding::Gzip => Encoding::Gzip,
            CachedEncoding::Brotli => Encoding::Brotli,
        };

        if cache_compressed_responses && (accepts_gzip || accepts_br) {
            let cached_body = {
                let guard = CACHED_FILES.read().await;
                let body = guard.get(&(path.clone(), desired_encoding)).copied();
                drop(guard);
                body
            };

            if let Some(cached_body) = cached_body {
                debug!("Found cached response for {}", path);
                response.set_header(Header::new(
                    CONTENT_ENCODING.as_str(),
                    format!("{}", encoding),
                ));
                response.set_sized_body(cached_body.len(), Cursor::new(cached_body));
                return;
            }
        }

        let body = response.body_mut().take();
        let compressed_body: Vec<u8> = match CompressionUtils::compress_body(body, desired_encoding)
            .await
        {
            Ok(compressed_body) => compressed_body,
            Err(err) => {
                error!("Failed to compress response body for {}; underlying `AsyncRead` likely failed: {}", path, err);
                response.set_streamed_body(ErrorBody(Some(err)));
                return;
            }
        };
        response.set_header(Header::new(
            CONTENT_ENCODING.as_str(),
            format!("{}", encoding),
        ));
        response.set_header(Header::new(
            CACHE_CONTROL.as_str(),
            "max-age=31536000"
        ));
        response.set_sized_body(compressed_body.len(), Cursor::new(compressed_body.clone()));

        debug!("Setting cached response for {}", path);
        CACHED_FILES
            .write()
            .await
            .insert((path, desired_encoding), Vec::leak(compressed_body));
    }
}
