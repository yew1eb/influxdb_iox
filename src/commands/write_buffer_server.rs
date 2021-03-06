#![deny(rust_2018_idioms)]

use tracing::{debug, info};

use std::env::VarError;
use std::fs;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::server::http_routes;
use crate::server::rpc::storage;

use ::storage::exec::Executor as StorageExecutor;
use hyper::service::{make_service_fn, service_fn};
use hyper::Server;
use write_buffer::{Db, WriteBufferDatabases};

pub async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    dotenv::dotenv().ok();

    let db_dir = match std::env::var("INFLUXDB_IOX_DB_DIR") {
        Ok(val) => val,
        Err(_) => {
            // default database path is $HOME/.influxdb_iox
            let mut path = dirs::home_dir().unwrap();
            path.push(".influxdb_iox/");
            path.into_os_string().into_string().unwrap()
        }
    };
    fs::create_dir_all(&db_dir)?;

    debug!("InfluxDB IOx Server using database directory: {:?}", db_dir);

    let storage = Arc::new(WriteBufferDatabases::new(&db_dir));
    let dirs = storage.wal_dirs()?;

    // TODO: make recovery of multiple databases multi-threaded
    for dir in dirs {
        let db = Db::restore_from_wal(dir).await?;
        storage.add_db(db).await;
    }

    // Fire up the query executor
    let executor = Arc::new(StorageExecutor::default());

    // Construct and start up gRPC server

    let grpc_bind_addr: SocketAddr = match std::env::var("INFLUXDB_IOX_GRPC_BIND_ADDR") {
        Ok(addr) => addr
            .parse()
            .expect("INFLUXDB_IOX_GRPC_BIND_ADDR environment variable not a valid SocketAddr"),
        Err(VarError::NotPresent) => "127.0.0.1:8082".parse().unwrap(),
        Err(VarError::NotUnicode(_)) => {
            panic!("INFLUXDB_IOX_GRPC_BIND_ADDR environment variable not a valid unicode string")
        }
    };

    let grpc_server = storage::make_server(grpc_bind_addr, storage.clone(), executor);

    info!("gRPC server listening on http://{}", grpc_bind_addr);

    // Construct and start up HTTP server

    let bind_addr: SocketAddr = match std::env::var("INFLUXDB_IOX_BIND_ADDR") {
        Ok(addr) => addr
            .parse()
            .expect("INFLUXDB_IOX_BIND_ADDR environment variable not a valid SocketAddr"),
        Err(VarError::NotPresent) => "127.0.0.1:8080".parse().unwrap(),
        Err(VarError::NotUnicode(_)) => {
            panic!("INFLUXDB_IOX_BIND_ADDR environment variable not a valid unicode string")
        }
    };

    let make_svc = make_service_fn(move |_conn| {
        let storage = storage.clone();
        async move {
            Ok::<_, http::Error>(service_fn(move |req| {
                let state = storage.clone();
                http_routes::service(req, state)
            }))
        }
    });

    let server = Server::bind(&bind_addr).serve(make_svc);
    info!("Listening on http://{}", bind_addr);

    println!("InfluxDB IOx server ready");

    // Wait for both the servers to complete
    let (grpc_server, server) = futures::future::join(grpc_server, server).await;

    grpc_server?;
    server?;

    Ok(())
}
