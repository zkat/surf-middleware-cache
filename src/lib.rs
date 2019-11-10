use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use http::HeaderMap;
use surf::middleware::{HttpClient, Middleware, Next, Request, Response};

#[async_trait]
pub trait CacheManager {
    async fn get(&self, req: &Request) -> Result<Option<Response>, surf::Exception>;
}

#[derive(Debug, PartialEq, Eq)]
pub enum CacheMode {
    Default,
    NoStore,
    Reload,
    NoCache,
    ForceCache,
    OnlyIfCached,
}

/// Caches requests according to http spec
#[derive(Debug)]
pub struct Cache<T: CacheManager> {
    mode: CacheMode,
    cache_manager: T,
}

impl<T: CacheManager> Cache<T> {
    pub async fn run<'a, C: HttpClient>(
        &self,
        req: Request,
        client: C,
        next: Next<'a, C>,
    ) -> Result<Response, surf::Exception> {
        let is_cacheable = (req.method() == http::Method::GET
            || req.method() == http::Method::HEAD)
            && self.mode != CacheMode::NoStore
            && self.mode != CacheMode::Reload;

        if !is_cacheable {
            return Ok(self.remote_fetch(req, client, next).await?);
        }

        if let Some(mut res) = self.cache_manager.get(&req).await? {
            if let Some(warning_code) = self.get_warning_code(&res) {
                // https://tools.ietf.org/html/rfc7234#section-4.3.4
                //
                // If a stored response is selected for update, the cache MUST:
                //
                // * delete any Warning header fields in the stored response with
                //   warn-code 1xx (see Section 5.5);
                //
                // * retain any Warning header fields in the stored response with
                //   warn-code 2xx;
                //
                if warning_code >= 100 && warning_code < 200 {
                    res.headers_mut().remove("Warning");
                }
            }

            if self.mode == CacheMode::Default && !self.is_stale(&req, &res) {
                Ok(res)
            } else if self.mode == CacheMode::Default {
                Ok(self.conditional_fetch(req, res, client, next).await?)
            } else if self.mode == CacheMode::ForceCache || self.mode == CacheMode::OnlyIfCached {
                //   112 Disconnected operation
                // SHOULD be included if the cache is intentionally disconnected from
                // the rest of the network for a period of time.
                // (https://tools.ietf.org/html/rfc2616#section-14.46)
                self.add_warning(&req, res.headers_mut(), 112, "Disconnected operation");
                Ok(res)
            } else {
                Ok(self.remote_fetch(req, client, next).await?)
            }
        } else if self.mode == CacheMode::OnlyIfCached {
            // ENOTCACHED
            unimplemented!()
        } else {
            Ok(self.remote_fetch(req, client, next).await?)
        }
    }

    fn get_warning_code(&self, res: &Response) -> Option<usize> {
        res.headers().get("Warning").and_then(|hdr| {
            hdr.to_str()
                .ok()
                .and_then(|s| s.chars().take(3).collect::<String>().parse().ok())
        })
    }

    fn is_stale(&self, req: &Request, res: &Response) -> bool {
        // TODO - most of what this looks like is gonna depend on http-cache-semantics
        unimplemented!()
    }

    fn clear_warnings(&self, headers: &mut HeaderMap) {
        headers.remove("Warning");
    }

    fn add_warning(&self, req: &Request, headers: &mut HeaderMap, code: usize, message: &str) {
        //   Warning    = "Warning" ":" 1#warning-value
        // warning-value = warn-code SP warn-agent SP warn-text [SP warn-date]
        // warn-code  = 3DIGIT
        // warn-agent = ( host [ ":" port ] ) | pseudonym
        //                 ; the name or pseudonym of the server adding
        //                 ; the Warning header, for use in debugging
        // warn-text  = quoted-string
        // warn-date  = <"> HTTP-date <">
        // (https://tools.ietf.org/html/rfc2616#section-14.46)
        //
        headers.append(
            "Warning",
            http::HeaderValue::from_str(
                format!(
                    "{} {} {:?} \"{}\"",
                    req.uri().host().expect("Invalid URL"),
                    code,
                    message,
                    // Close enough to RFC1123 (HTTP-date)
                    Utc::now().to_rfc2822()
                )
                .as_str(),
            )
            .expect("Failed to generate warning string"),
        );
    }

    async fn conditional_fetch<'a, C: HttpClient>(
        &self,
        req: Request,
        res: Response,
        client: C,
        next: Next<'a, C>,
    ) -> Result<Response, surf::Exception> {
        unimplemented!()
    }

    async fn remote_fetch<'a, C: HttpClient>(
        &self,
        req: Request,
        client: C,
        next: Next<'a, C>,
    ) -> Result<Response, surf::Exception> {
        Ok(next.run(req, client).await?)
    }
}

// impl<C: HttpClient, T: CacheManager> Middleware<C> for Cache<T> {
//     fn handle<'a>(
//         &'a self,
//         req: Request,
//         client: C,
//         next: Next<'a, C>,
//     ) -> BoxFuture<'a, Result<Response, surf::Exception>> {
//         Box::pin(async move { Ok(self.run(req, client, next).await?) })
//     }
// }
