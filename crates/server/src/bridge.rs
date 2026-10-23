//! `soundcheck mcp`: the plugin's MCP server. Claude Code connects plugin servers once, at session start, usually
//! before any run is serving, so this answers over stdio from the start and forwards each tool call to whichever
//! run is serving on the port.

use crate::serve::tools::{INSTRUCTIONS, Tools};
use anyhow::Result;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ClientConfig, ContentBlock,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
};
use rmcp::service::{RequestContext, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ErrorData, RoleClient, RoleServer, ServerHandler, ServiceError, ServiceExt};
use tokio::sync::Mutex;

pub async fn bridge(port: u16) -> Result<()> {
    let bridge = Bridge {
        url: format!("http://127.0.0.1:{port}/mcp"),
        run: Mutex::new(None),
    };
    bridge
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

struct Bridge {
    url: String,
    run: Mutex<Option<RunningService<RoleClient, ClientConfig>>>,
}

impl Bridge {
    async fn connect(&self) -> Result<rmcp::Peer<RoleClient>, String> {
        let mut run = self.run.lock().await;
        if let Some(service) = run.as_ref().filter(|s| !s.is_transport_closed()) {
            return Ok(service.peer().clone());
        }
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(self.url.clone()),
        );
        let service = ClientConfig::default()
            .serve(transport)
            .await
            .map_err(|e| {
                format!(
                    "no soundcheck run is serving at {} ({e}); start one with `soundcheck run`",
                    self.url
                )
            })?;
        // A bridge outlives updates (Claude Code keeps it for the session), so an old one would drive a new server
        // with the old tool list.
        let theirs = service
            .peer_info()
            .and_then(|info| info.server_info.as_ref().map(|s| s.version.clone()))
            .unwrap_or_default();
        if theirs != crate::VERSION {
            return Err(format!(
                "this session's soundcheck connection is {} but the run's server is {theirs}: reconnect soundcheck with /mcp",
                crate::VERSION
            ));
        }
        let peer = service.peer().clone();
        *run = Some(service);
        Ok(peer)
    }

    async fn forward(&self, params: CallToolRequestParams) -> Result<CallToolResponse, String> {
        let peer = self.connect().await?;
        match peer.call_tool_once(params.clone()).await {
            // The request never reached a server (a restarted run has a new session), so sending it again is safe.
            Err(ServiceError::TransportSend(_) | ServiceError::TransportClosed) => {
                self.run.lock().await.take();
                let peer = self.connect().await?;
                peer.call_tool_once(params).await.map_err(|e| e.to_string())
            }
            other => other.map_err(|e| e.to_string()),
        }
    }
}

impl ServerHandler for Bridge {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(INSTRUCTIONS)
            .with_server_info(rmcp::model::Implementation::new(
                "soundcheck",
                crate::VERSION,
            ))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(
            Tools::tool_router().list_all(),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        Ok(match self.forward(request).await {
            Ok(response) => response,
            Err(e) => {
                CallToolResult::error(vec![ContentBlock::text(format!("refused: {e}"))]).into()
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lists_the_tools_and_refuses_calls_with_no_run_serving() {
        let (ours, theirs) = tokio::io::duplex(64 * 1024);
        let bridge = Bridge {
            url: "http://127.0.0.1:9/mcp".into(),
            run: Mutex::new(None),
        };
        tokio::spawn(async move { bridge.serve(theirs).await.unwrap().waiting().await });
        let client = ClientConfig::default().serve(ours).await.unwrap();

        let tools = client.list_all_tools().await.unwrap();
        assert!(tools.iter().any(|t| t.name == "dispatch"));
        assert!(tools.iter().any(|t| t.name == "post_result"));

        let result = client
            .call_tool(CallToolRequestParams::new("status"))
            .await
            .unwrap();
        let text = result.content[0].as_text().unwrap().text.clone();
        assert!(text.contains("no soundcheck run is serving"), "{text}");
    }
}
