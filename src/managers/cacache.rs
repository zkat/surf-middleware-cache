use std::collections::HashMap;

use crate::CacheManager;

use http_cache_semantics::CachePolicy;
use serde::{Deserialize, Serialize};
use surf::{Request, Response};

type Result<T> = std::result::Result<T, http_types::Error>;

pub struct CACacheManager {
    path: String,
}

impl Default for CACacheManager {
    fn default() -> Self {
        CACacheManager {
            path: "./surf-cacache".into(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct Store {
    response: StoredResponse,
    policy: CachePolicy,
}

#[derive(Debug, Deserialize, Serialize)]
struct StoredResponse {
    body: Vec<u8>,
    headers: HashMap<String, String>,
}

async fn to_store(res: &mut Response, policy: CachePolicy) -> Result<Store> {
    let mut headers = HashMap::new();
    for header in res.iter() {
        headers.insert(header.0.as_str().to_owned(), header.1.as_str().to_owned());
    }
    let body: Vec<u8> = res.body_bytes().await?;
    Ok(Store {
        response: StoredResponse { body, headers },
        policy,
    })
}

fn from_store(store: &Store) -> Response {
    let mut res = http_types::Response::new(http_types::StatusCode::Ok);
    for header in &store.response.headers {
        let val =
            http_types::headers::HeaderValue::from_bytes(header.1.as_bytes().to_vec()).unwrap();
        res.insert_header(header.0.as_str(), val);
    }
    res.set_body(store.response.body.clone());
    Response::from(res)
}

fn req_key(req: &Request) -> String {
    format!("{}:{}", req.method(), req.url())
}

#[allow(dead_code)]
impl CACacheManager {
    async fn clear(&self) -> Result<()> {
        cacache::clear(&self.path).await?;
        Ok(())
    }
}

#[surf::utils::async_trait]
impl CacheManager for CACacheManager {
    async fn get(&self, req: &Request) -> Result<Option<(Response, CachePolicy)>> {
        let store: Store = match cacache::read(&self.path, &req_key(req)).await {
            Ok(d) => bincode::deserialize(&d)?,
            Err(_e) => {
                return Ok(None);
            }
        };
        Ok(Some((from_store(&store), store.policy)))
    }

    // TODO - This needs some reviewing.
    async fn put(
        &self,
        req: &Request,
        res: &mut Response,
        policy: CachePolicy,
    ) -> Result<Response> {
        let data = to_store(res, policy).await?;
        let bytes = bincode::serialize(&data).unwrap();
        cacache::write(&self.path, &req_key(req), bytes).await?;
        let mut ret_res = http_types::Response::new(res.status());
        ret_res.set_body(res.body_bytes().await?);
        for header in res.iter() {
            ret_res.insert_header(header.0, header.1);
        }
        ret_res.set_version(res.version());
        Ok(Response::from(ret_res))
    }

    async fn delete(&self, req: &Request) -> Result<()> {
        Ok(cacache::remove(&self.path, &req_key(req)).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{get_request_parts, get_response_parts};
    use http_types::{Method, Response, StatusCode};
    use std::str::FromStr;
    use surf::{Request, Result};

    #[async_std::test]
    async fn can_cache_response() -> Result<()> {
        let url = surf::http::Url::from_str("https://example.com")?;
        let mut res = Response::new(StatusCode::Ok);
        res.set_body("test");
        let mut res = surf::Response::from(res);
        let req = Request::new(Method::Get, url);
        let policy = CachePolicy::new(&get_request_parts(&req), &get_response_parts(&res));
        let manager = CACacheManager::default();
        manager.put(&req, &mut res, policy).await?;
        let data = manager.get(&req).await?;
        let body = match data {
            Some(mut d) => d.0.body_string().await?,
            None => String::new(),
        };
        assert_eq!(&body, "test");
        manager.delete(&req).await?;
        let data = manager.get(&req).await?;
        assert!(data.is_none());
        manager.clear().await?;
        Ok(())
    }
}
