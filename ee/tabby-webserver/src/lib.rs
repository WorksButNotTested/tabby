pub mod api;

mod schema;
pub use schema::create_schema;
use tabby_common::api::{
    code::{CodeSearch, SearchResponse},
    event::RawEventLogger,
};
use tracing::{error, warn};
use websocket::WebSocketTransport;

mod db;
mod server;
mod ui;
mod websocket;

use std::{net::SocketAddr, sync::Arc};

use api::{Hub, HubError, Worker, WorkerKind};
use axum::{
    extract::{ws::WebSocket, ConnectInfo, State, WebSocketUpgrade},
    http::Request,
    middleware::{from_fn_with_state, Next},
    response::IntoResponse,
    routing, Extension, Router,
};
use hyper::Body;
use juniper_axum::{graphiql, graphql, playground};
use schema::Schema;
use server::ServerContext;
use tarpc::server::{BaseChannel, Channel};

pub async fn attach_webserver(
    api: Router,
    ui: Router,
    logger: Arc<dyn RawEventLogger>,
    code: Arc<dyn CodeSearch>,
) -> (Router, Router) {
    let conn = db::DbConn::new().await.unwrap();
    let ctx = Arc::new(ServerContext::new(conn, logger, code));
    let schema = Arc::new(create_schema());

    let api = api
        .layer(from_fn_with_state(ctx.clone(), distributed_tabby_layer))
        .route(
            "/graphql",
            routing::post(graphql::<Arc<Schema>>).with_state(ctx.clone()),
        )
        .layer(Extension(schema))
        .route("/hub", routing::get(ws_handler).with_state(ctx));

    let ui = ui
        .route("/graphql", routing::get(playground("/graphql", None)))
        .route("/graphiql", routing::get(graphiql("/graphql", None)))
        .fallback(ui::handler);

    (api, ui)
}

async fn distributed_tabby_layer(
    State(ws): State<Arc<ServerContext>>,
    request: Request<Body>,
    next: Next<Body>,
) -> axum::response::Response {
    ws.dispatch_request(request, next).await
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<ServerContext>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(state, socket, addr))
}

async fn handle_socket(state: Arc<ServerContext>, socket: WebSocket, addr: SocketAddr) {
    let transport = WebSocketTransport::from(socket);
    let server = BaseChannel::with_defaults(transport);
    let imp = Arc::new(HubImpl::new(state.clone(), addr));
    tokio::spawn(server.execute(imp.serve())).await.unwrap()
}

pub struct HubImpl {
    ctx: Arc<ServerContext>,
    conn: SocketAddr,
}

impl HubImpl {
    pub fn new(ctx: Arc<ServerContext>, conn: SocketAddr) -> Self {
        Self { ctx, conn }
    }
}

#[tarpc::server]
impl Hub for Arc<HubImpl> {
    async fn register_worker(
        self,
        _context: tarpc::context::Context,
        kind: WorkerKind,
        port: i32,
        name: String,
        device: String,
        arch: String,
        cpu_info: String,
        cpu_count: i32,
        cuda_devices: Vec<String>,
        token: String,
    ) -> Result<Worker, HubError> {
        if token.is_empty() {
            return Err(HubError::InvalidToken("Empty worker token".to_string()));
        }
        let server_token = match self.ctx.read_registration_token().await {
            Ok(t) => t,
            Err(err) => {
                error!("fetch server token: {}", err.to_string());
                return Err(HubError::InvalidToken(
                    "Failed to fetch server token".to_string(),
                ));
            }
        };
        if server_token != token {
            return Err(HubError::InvalidToken("Token mismatch".to_string()));
        }

        let worker = Worker {
            name,
            kind,
            addr: format!("http://{}:{}", self.conn.ip(), port),
            device,
            arch,
            cpu_info,
            cpu_count,
            cuda_devices,
        };
        self.ctx.register_worker(worker).await
    }

    async fn log_event(self, _context: tarpc::context::Context, content: String) {
        self.ctx.logger.log(content)
    }

    async fn search(
        self,
        _context: tarpc::context::Context,
        q: String,
        limit: usize,
        offset: usize,
    ) -> SearchResponse {
        match self.ctx.code.search(&q, limit, offset).await {
            Ok(serp) => serp,
            Err(err) => {
                warn!("Failed to search: {}", err);
                SearchResponse::default()
            }
        }
    }

    async fn search_in_language(
        self,
        _context: tarpc::context::Context,
        language: String,
        tokens: Vec<String>,
        limit: usize,
        offset: usize,
    ) -> SearchResponse {
        match self
            .ctx
            .code
            .search_in_language(&language, &tokens, limit, offset)
            .await
        {
            Ok(serp) => serp,
            Err(err) => {
                warn!("Failed to search: {}", err);
                SearchResponse::default()
            }
        }
    }
}
