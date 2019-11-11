use std::time::SystemTime;

use async_trait::async_trait;
use futures::future::BoxFuture;
use http::HeaderMap;
use httpdate;
use surf::middleware::{Body, HttpClient, Middleware, Next, Request, Response};

#[async_trait]
pub trait CacheManager {
    async fn get(&self, req: &Request) -> Result<Option<Response>, surf::Exception>;
    async fn put(&self, req: &Request, res: Response) -> Result<Response, surf::Exception>;
    async fn delete(&self, req: &Request) -> Result<(), surf::Exception>;
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
            if let Some(warning_code) = get_warning_code(&res) {
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

            if self.mode == CacheMode::Default && !is_stale(&req, &res) {
                Ok(res)
            } else if self.mode == CacheMode::Default {
                Ok(self.conditional_fetch(req, res, client, next).await?)
            } else if self.mode == CacheMode::ForceCache || self.mode == CacheMode::OnlyIfCached {
                //   112 Disconnected operation
                // SHOULD be included if the cache is intentionally disconnected from
                // the rest of the network for a period of time.
                // (https://tools.ietf.org/html/rfc2616#section-14.46)
                add_warning(&req.uri(), res.headers_mut(), 112, "Disconnected operation");
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

    async fn conditional_fetch<'a, C: HttpClient>(
        &self,
        mut req: Request,
        mut cached_res: Response,
        client: C,
        next: Next<'a, C>,
    ) -> Result<Response, surf::Exception> {
        set_revalidation_headers(&mut req);
        let copied_req = clone_req(&req);
        match self.remote_fetch(req, client, next).await {
            Ok(cond_res) => {
                if cond_res.status().is_server_error() && must_revalidate(&cached_res) {
                    //   111 Revalidation failed
                    //   MUST be included if a cache returns a stale response
                    //   because an attempt to revalidate the response failed,
                    //   due to an inability to reach the server.
                    // (https://tools.ietf.org/html/rfc2616#section-14.46)
                    add_warning(
                        &copied_req.uri(),
                        cached_res.headers_mut(),
                        111,
                        "Revalidation failed",
                    );
                    Ok(cached_res)
                } else if cond_res.status() == http::StatusCode::NOT_MODIFIED {
                    let mut res = http::Response::builder();
                    res.status(cond_res.status());
                    let headers = res.headers_mut().expect("Couldn't get headers.");
                    for (key, value) in cond_res.headers().into_iter() {
                        headers.append(key, value.clone());
                    }
                    // TODO - set headers to revalidated response headers? Needs http-cache-semantics.
                    let res = res.body(cached_res.into_body()).unwrap();
                    let res = self.cache_manager.put(&copied_req, res).await?;
                    Ok(res)
                } else {
                    Ok(cached_res)
                }
            }
            Err(e) => {
                if must_revalidate(&cached_res) {
                    Err(e)
                } else {
                    let mut headers = cached_res.headers_mut();
                    //   111 Revalidation failed
                    //   MUST be included if a cache returns a stale response
                    //   because an attempt to revalidate the response failed,
                    //   due to an inability to reach the server.
                    // (https://tools.ietf.org/html/rfc2616#section-14.46)
                    add_warning(&copied_req.uri(), &mut headers, 111, "Revalidation failed");
                    //   199 Miscellaneous warning
                    //   The warning text MAY include arbitrary information to
                    //   be presented to a human user, or logged. A system
                    //   receiving this warning MUST NOT take any automated
                    //   action, besides presenting the warning to the user.
                    // (https://tools.ietf.org/html/rfc2616#section-14.46)
                    add_warning(
                        &copied_req.uri(),
                        &mut headers,
                        199,
                        format!("Miscellaneous Warning {}", e).as_str(),
                    );

                    Ok(cached_res)
                }
            }
        }
    }

    async fn remote_fetch<'a, C: HttpClient>(
        &self,
        req: Request,
        client: C,
        next: Next<'a, C>,
    ) -> Result<Response, surf::Exception> {
        let copied_req = clone_req(&req);
        let res = next.run(req, client).await?;
        let is_method_get_head =
            copied_req.method() == http::Method::GET || copied_req.method() == http::Method::HEAD;
        let is_cacheable = self.mode != CacheMode::NoStore
            && is_method_get_head
            && res.status() == http::StatusCode::OK;
        // TODO
        // && policy.is_storable(&req_copy, &res);
        if is_cacheable {
            Ok(self.cache_manager.put(&copied_req, res).await?)
        } else if !is_method_get_head {
            self.cache_manager.delete(&copied_req).await?;
            Ok(res)
        } else {
            Ok(res)
        }
    }
}

fn must_revalidate(res: &Response) -> bool {
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

fn set_revalidation_headers(mut req: &Request) {
    // TODO - need http-cache-semantics to do this.
    unimplemented!()
}

fn get_warning_code(res: &Response) -> Option<usize> {
    res.headers().get("Warning").and_then(|hdr| {
        hdr.to_str()
            .ok()
            .and_then(|s| s.chars().take(3).collect::<String>().parse().ok())
    })
}

fn is_stale(req: &Request, res: &Response) -> bool {
    // TODO - most of what this looks like is gonna depend on http-cache-semantics
    unimplemented!()
}

fn add_warning(uri: &http::Uri, headers: &mut HeaderMap, code: usize, message: &str) {
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

fn clone_req(req: &Request) -> Request {
    let mut copied_req = http::Request::new(Body::empty());
    *copied_req.method_mut() = req.method().clone();
    *copied_req.uri_mut() = req.uri().clone();
    *copied_req.headers_mut() = req.headers().clone();
    *copied_req.version_mut() = req.version().clone();
    copied_req
}

// TODO - Surf needs to resolve an issue with Body not being Sync
//        Ref: https://github.com/http-rs/surf/issues/97
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
