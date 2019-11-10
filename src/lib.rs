use std::time::SystemTime;

use async_trait::async_trait;
use futures::future::BoxFuture;
use http::HeaderMap;
use httpdate;
use surf::middleware::{HttpClient, Body, Middleware, Next, Request, Response};

#[async_trait]
pub trait CacheManager {
    async fn get(&self, req: &Request) -> Result<Option<Response>, surf::Exception>;
    async fn put(&self, req: &Request, res: Response) -> Result<Response, surf::Exception>;
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
                self.add_warning(&req.uri(), res.headers_mut(), 112, "Disconnected operation");
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

    fn add_warning(&self, uri: &http::Uri, headers: &mut HeaderMap, code: usize, message: &str) {
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
                    uri.host().expect("Invalid URL"),
                    code,
                    message,
                    httpdate::fmt_http_date(SystemTime::now())
                )
                .as_str(),
            )
            .expect("Failed to generate warning string"),
        );
    }

    async fn conditional_fetch<'a, C: HttpClient>(
        &self,
        mut req: Request,
        mut cached_res: Response,
        client: C,
        next: Next<'a, C>,
    ) -> Result<Response, surf::Exception> {
        self.set_revalidation_headers(&mut req);
        let uri = req.uri().clone();
        let req_headers = req.headers().clone();
        match self.remote_fetch(req, client, next).await {
            Ok(cond_res) => {
                if cond_res.status().is_server_error() && self.must_revalidate(&cached_res) {
                    //   111 Revalidation failed
                    //   MUST be included if a cache returns a stale response
                    //   because an attempt to revalidate the response failed,
                    //   due to an inability to reach the server.
                    // (https://tools.ietf.org/html/rfc2616#section-14.46)
                    self.add_warning(&uri, cached_res.headers_mut(), 111, "Revalidation failed");
                    Ok(cached_res)
                } else if cond_res.status() == http::StatusCode::NOT_MODIFIED {
                    // TODO - simplify/extract into a function? This is super verbose.
                    let mut res = http::Response::builder();
                    res.status(cond_res.status());
                    let headers = res.headers_mut().expect("Couldn't get headers.");
                    for (key, value) in cond_res.headers().into_iter() {
                        headers.append(key, value.clone());
                    }
                    let res = res.body(cached_res.into_body()).unwrap();
                    let mut dummy_req = http::Request::builder();
                    dummy_req.uri(uri);
                    for (key, value) in req_headers.into_iter() {
                        dummy_req.header(key.unwrap(), value.clone());
                    }
                    // TODO - set headers to revalidated response headers? Needs http-cache-semantics.
                    let dummy_req = dummy_req.body(Body::empty()).unwrap();
                    let res = self.cache_manager.put(&dummy_req, res).await?;
                    Ok(res)
                } else {
                    Ok(cached_res)
                }
            }
            Err(e) => {
                if self.must_revalidate(&cached_res) {
                    Err(e)
                } else {
                    let mut headers = cached_res.headers_mut();
                    //   111 Revalidation failed
                    //   MUST be included if a cache returns a stale response
                    //   because an attempt to revalidate the response failed,
                    //   due to an inability to reach the server.
                    // (https://tools.ietf.org/html/rfc2616#section-14.46)
                    self.add_warning(&uri, &mut headers, 111, "Revalidation failed");
                    //   199 Miscellaneous warning
                    //   The warning text MAY include arbitrary information to
                    //   be presented to a human user, or logged. A system
                    //   receiving this warning MUST NOT take any automated
                    //   action, besides presenting the warning to the user.
                    // (https://tools.ietf.org/html/rfc2616#section-14.46)
                    self.add_warning(&uri, &mut headers, 199, format!("Miscellaneous Warning {}", e).as_str());

                    Ok(cached_res)
                }
            }
        }
    }

    fn set_revalidation_headers(&self, mut req: &Request) {
        // TODO - need http-cache-semantics to do this.
        unimplemented!()
    }

    fn must_revalidate(&self, res: &Response) -> bool {
        if let Some(val) = res
            .headers()
            .get("Cache-Control")
            .and_then(|h| h.to_str().ok())
        {
            val.to_lowercase().contains("must-revalidate")
        } else {
            false
        }
    }

    async fn remote_fetch<'a, C: HttpClient>(
        &self,
        req: Request,
        client: C,
        next: Next<'a, C>,
    ) -> Result<Response, surf::Exception> {
        // TODO - cache
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
