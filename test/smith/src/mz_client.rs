// Copyright Materialize, Inc. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::HashMap;
use std::marker::Sync;

use postgres_types::ToSql;
use serde::Serialize;
use tokio_postgres::{Client, NoTls};

use crate::error::Result;

#[derive(Clone, Debug, Serialize)]
pub struct Column {
    pub data_type: String,
    pub name: String,
}

/// A materialized client with custom methods
pub struct MzClient(Client);

impl MzClient {
    /// Construct a new client talking to mz_url
    pub async fn new(mz_host: &str, mz_port: u16) -> Result<MzClient> {
        let (client, conn) = tokio_postgres::Config::new()
            .user("mzd")
            .host(mz_host)
            .port(mz_port)
            .connect(NoTls)
            .await?;

        // The connection object performs the actual communication with the database,
        // so spawn it off to run on its own.
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                log::error!("connection error: {}", e);
            }
        });

        Ok(MzClient(client))
    }

    pub async fn execute(&self, query: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64> {
        log::debug!("exec-> {} params={:?}", query, params);
        Ok(self.0.execute(query, params).await?)
    }

    pub async fn show_views(&self) -> Result<String> {
        let mut v: HashMap<String, Vec<Column>> = HashMap::new();
        for row in self.0.query("SHOW VIEWS", &[]).await? {
            let view: String = row.get(0);

            let columns = self.print_columns(&view).await?;
            v.insert(view, columns);
        }

        Ok(serde_json::to_string(&v)?)
    }

    pub async fn print_columns(&self, view: &str) -> Result<Vec<Column>> {
        let mut res = Vec::new();
        let q = format!("SHOW COLUMNS IN {}", view);
        for row in self.0.query(&*q, &[]).await? {
            let name: String = row.get(0);

            // TODO cleaner way to map our types to types Smith understands
            res.push(Column {
                data_type: "int".to_string(),
                name,
            });
        }

        Ok(res)
    }
}
