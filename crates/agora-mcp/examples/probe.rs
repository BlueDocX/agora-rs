//! List a remote MCP server's tools, optionally call one:
//! `cargo run -p agora-mcp --example probe -- <url> [tool '{"arg": "value"}']`
//! (a token, if the server needs one, from `MCP_TOKEN`).

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let url = args.next().ok_or("usage: probe <url> [tool json]")?;
    let server = agora_mcp::Server { url, token: std::env::var("MCP_TOKEN").ok(), allow_local: false };
    for t in agora_mcp::list_tools(&server).await? {
        println!("{} (read-only: {:?}, destructive: {:?}) {}", t.name, t.read_only, t.destructive, t.description.chars().take(90).collect::<String>());
    }
    if let (Some(tool), Some(json)) = (args.next(), args.next()) {
        let reply = agora_mcp::call(&server, &tool, &serde_json::from_str(&json)?).await?;
        println!("\n{tool} → ok: {}\n{}", reply.ok, reply.text.chars().take(600).collect::<String>());
    }
    Ok(())
}
