use std::{str::FromStr, time::SystemTime};

use http_types::{headers::HeaderValue, Method};
use surf::{
    middleware::{Middleware, Next},
    Client, Request, Response,
};

#[surf::utils::async_trait]
pub trait CacheManager {
    async fn get(&self, req: &Request) -> Result<Option<Response>, http_types::Error>;
    async fn put(&self, req: &Request, res: Response) -> Result<Response, http_types::Error>;
    async fn delete(&self, req: &Request) -> Result<(), http_types::Error>;
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
    pub async fn run(
        &self,
        req: Request,
        client: Client,
        next: Next<'_>,
    ) -> Result<Response, http_types::Error> {
        let is_cacheable = (req.method() == Method::Get || req.method() == Method::Head)
            && self.mode != CacheMode::NoStore
            && self.mode != CacheMode::Reload;

        if !is_cacheable {
            return self.remote_fetch(req, client, next).await;
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
                if (100..200).contains(&warning_code) {
                    res.remove_header("Warning");
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
                res.append_header(
                    "Warning",
                    build_warning(req.url(), 112, "Disconnected operation"),
                );
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

    async fn conditional_fetch(
        &self,
        mut req: Request,
        mut cached_res: Response,
        client: Client,
        next: Next<'_>,
    ) -> Result<Response, http_types::Error> {
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
                    cached_res.append_header(
                        "Warning",
                        build_warning(copied_req.url(), 111, "Revalidation failed"),
                    );
                    Ok(cached_res)
                } else if cond_res.status() == http_types::StatusCode::NotModified {
                    let mut res = http_types::Response::new(cond_res.status());
                    for (key, value) in cond_res.iter() {
                        res.append_header(key, value.clone().as_str());
                    }
                    // TODO - set headers to revalidated response headers? Needs http-cache-semantics.
                    res.set_body(cached_res.body_string().await?);
                    let res = self.cache_manager.put(&copied_req, res.into()).await?;
                    Ok(res)
                } else {
                    Ok(cached_res)
                }
            }
            Err(e) => {
                if must_revalidate(&cached_res) {
                    Err(e)
                } else {
                    //   111 Revalidation failed
                    //   MUST be included if a cache returns a stale response
                    //   because an attempt to revalidate the response failed,
                    //   due to an inability to reach the server.
                    // (https://tools.ietf.org/html/rfc2616#section-14.46)
                    cached_res.append_header(
                        "Warning",
                        build_warning(copied_req.url(), 111, "Revalidation failed"),
                    );
                    //   199 Miscellaneous warning
                    //   The warning text MAY include arbitrary information to
                    //   be presented to a human user, or logged. A system
                    //   receiving this warning MUST NOT take any automated
                    //   action, besides presenting the warning to the user.
                    // (https://tools.ietf.org/html/rfc2616#section-14.46)
                    cached_res.append_header(
                        "Warning",
                        build_warning(
                            copied_req.url(),
                            199,
                            format!("Miscellaneous Warning {}", e).as_str(),
                        ),
                    );

                    Ok(cached_res)
                }
            }
        }
    }

    async fn remote_fetch(
        &self,
        req: Request,
        client: Client,
        next: Next<'_>,
    ) -> Result<Response, http_types::Error> {
        let copied_req = clone_req(&req);
        let res = next.run(req, client).await?;
        let is_method_get_head =
            copied_req.method() == Method::Get || copied_req.method() == Method::Head;
        let is_cacheable = self.mode != CacheMode::NoStore
            && is_method_get_head
            && res.status() == http_types::StatusCode::Ok;
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
    if let Some(val) = res.header("Cache-Control") {
        val.as_str().to_lowercase().contains("must-revalidate")
    } else {
        false
    }
}

fn set_revalidation_headers(mut _req: &Request) {
    // TODO - need http-cache-semantics to do this.
    unimplemented!()
}

fn get_warning_code(res: &Response) -> Option<usize> {
    res.header("Warning").and_then(|hdr| {
        hdr.as_str()
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
    })
}

fn is_stale(_req: &Request, _res: &Response) -> bool {
    // TODO - most of what this looks like is gonna depend on http-cache-semantics
    unimplemented!()
}

fn build_warning(uri: &surf::http::Url, code: usize, message: &str) -> HeaderValue {
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
    HeaderValue::from_str(
        format!(
            "{} {} {:?} \"{}\"",
            uri.host().expect("Invalid URL"),
            code,
            message,
            httpdate::fmt_http_date(SystemTime::now())
        )
        .as_str(),
    )
    .expect("Failed to generate warning string")
}

fn clone_req(req: &Request) -> Request {
    let mut copied_req = http_types::Request::new(req.method(), req.url().clone());
    for (key, value) in req.iter() {
        copied_req.insert_header(key, value.clone().as_str());
    }
    // TODO - Didn't see where to pull version from surf::Request
    // copied_req.set_version(req.version().clone());
    copied_req.into()
}

#[surf::utils::async_trait]
impl<T: CacheManager + 'static + Send + Sync> Middleware for Cache<T> {
    async fn handle(
        &self,
        req: Request,
        client: Client,
        next: Next<'_>,
    ) -> Result<Response, http_types::Error> {
        let res = next.run(req, client).await?;
        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_types::{Response, StatusCode};
    use surf::Result;

    #[async_std::test]
    async fn can_get_warning_code() -> Result<()> {
        let url = surf::http::Url::from_str("https://example.com")?;
        let mut res = Response::new(StatusCode::Ok);
        res.append_header("Warning", build_warning(&url, 111, "Revalidation failed"));
        let code = get_warning_code(&res.into()).unwrap();
        Ok(assert_eq!(code, 111))
    }

    #[async_std::test]
    async fn can_check_revalidate() {
        let mut res = Response::new(StatusCode::Ok);
        res.append_header("Cache-Control", "max-age=1733992, must-revalidate");
        let check = must_revalidate(&res.into());
        assert_eq!(check, true)
    }
}
