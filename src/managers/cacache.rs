use crate::CacheManager;

use surf::{Request, Response};

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

fn req_key(req: &Request) -> String {
    format!("{}:{}", req.method(), req.url())
}

#[surf::utils::async_trait]
impl CacheManager for CACacheManager {
    async fn get(&self, req: &Request) -> Result<Option<Response>, http_types::Error> {
        unimplemented!()
    }

    async fn put(&self, req: &Request, res: Response) -> Result<Response, http_types::Error> {
        unimplemented!()
    }

    async fn delete(&self, req: &Request) -> Result<(), http_types::Error> {
        unimplemented!()
    }
}
