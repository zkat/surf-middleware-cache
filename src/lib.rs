use async_trait::async_trait;
use futures::future::BoxFuture;
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
    async fn run<'a, C: HttpClient>(
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
                    res.headers().remove("Warning");
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
                self.set_warning(&res, 112, "Disconnected operation");
                Ok(res)
            } else {
                Ok(self.remote_fetch(req, client, next).await?)
            }
        } else if self.mode == CacheMode::OnlyIfCached {
            // ENOTCACHED
            Err(surf::Exception)
        } else {
            Ok(self.remote_fetch(req, client, next).await?)
        }
    }

    fn get_warning_code(&self, res: &Response) -> Option<usize> {
        unimplemented!()
    }

    fn is_stale(&self, req: &Request, res: &Response) -> bool {
        unimplemented!()
    }

    fn set_warning(&self, res: &Response, code: usize, message: &str) {
        unimplemented!()
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
        unimplemented!()
    }
}

impl<C: HttpClient, T: CacheManager> Middleware<C> for Cache<T> {
    fn handle<'a>(
        &'a self,
        req: Request,
        client: C,
        next: Next<'a, C>,
    ) -> BoxFuture<'a, Result<Response, surf::Exception>> {
        Box::pin(async move { Ok(self.run(req, client, next).await?) })
    }
}
