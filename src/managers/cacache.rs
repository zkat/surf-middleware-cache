use std::collections::HashMap;

use crate::CacheManager;

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
struct ResponseStore {
    body: Vec<u8>,
    headers: HashMap<String, String>,
}

async fn to_store(res: &mut Response) -> Result<ResponseStore> {
    let mut headers = HashMap::new();
    for header in res.iter() {
        headers.insert(header.0.as_str().to_owned(), header.1.as_str().to_owned());
    }
    let body: Vec<u8> = res.body_bytes().await?;
    Ok(ResponseStore { body, headers })
}

fn from_store(store: ResponseStore) -> Response {
    let mut res = http_types::Response::new(http_types::StatusCode::Ok);
    for header in store.headers {
        let val =
            http_types::headers::HeaderValue::from_bytes(header.1.as_bytes().to_vec()).unwrap();
        res.insert_header(header.0.as_str(), val);
    }
    res.set_body(store.body);
    Response::from(res)
}

fn req_key(req: &Request) -> String {
    format!("{}:{}", req.method(), req.url())
}

#[surf::utils::async_trait]
impl CacheManager for CACacheManager {
    async fn get(&self, req: &Request) -> Result<Option<Response>> {
        let store: ResponseStore = match cacache::read(&self.path, &req_key(req)).await {
            Ok(d) => {
                bincode::deserialize(&d)?
            },
            Err(_e) => {
                return Ok(None);
            }
        };
        Ok(Some(from_store(store)))
    }

    // TODO - This needs some reviewing.
    async fn put(&self, req: &Request, res: &mut Response) -> Result<Response> {
        let data = to_store(res).await?;
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
