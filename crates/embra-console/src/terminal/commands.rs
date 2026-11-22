//! Slash command handling for embra-console.
//!
//! Only local commands (/help, /ml, /media, /show-tools) are handled
//! here. All other commands are forwarded to embra-brain via gRPC.

/// Returns true if this command is handled locally (not sent to brain)
pub fn is_local_command(cmd: &str) -> bool {
    matches!(cmd, "/help" | "/ml" | "/media" | "/show-tools")
}

/// Handle a local command, returning the output string
pub fn handle_local_command(cmd: &str, _args: &str, name: &str) -> Option<String> {
    match cmd {
        "/help" => Some(format!(
            r#"=== {} Help ===

Session:
  /sessions                      List all sessions
  /new <name>                    Create a new session
  /switch <name>                 Switch to a session
  /close                         Close current session
  /sessions delete <name>        Guided delete: summary + reason + memories, then 7-day-grace soft delete
  /sessions restore <name>       Undo a soft delete during its grace period

Turn:
  /ml                            Toggle multi-line mode (. on own line to send)
  /stop                          Stop a stuck in-flight turn (or press Esc while it runs)
  /continue                      Send the last call of a turn that a failed model call cut once more, with everything the turn held
  /iter-cap                      Show the per-turn tool iteration cap
  /iter-cap <N>                  Set the cap (1..=1000, default 100)
  /iter-cap reset                Restore the default cap
  /show-reasoning                Show whether reasoning streams to the panel
  /show-reasoning <on|off>       Toggle live reasoning in the expression panel (default on)
  /show-reasoning reset          Restore default (on)
  /show-tools                    Show how tool calls render here (web console default: marker; serial: full)
  /show-tools <full|marker|off>  The whole call, one marker line per call, or nothing (the web console's Tools tab keeps every call)

Model:
  /provider                              Show active provider, model, session
  /provider <anthropic|gemini|ollama|lm_studio|native>
                                         Switch provider for future turns (native: a model on this machine, experimental)
  /provider --setup <anthropic|gemini>   Add/replace an API key, then pick the model (multi-turn)
  /provider --setup <ollama|lm_studio>   Reconfigure endpoint, bearer, model (multi-turn)
  /provider --setup native               Pick a native model (experimental); its file is downloaded first when it is not installed
  /model                                 Show the model; Ollama / LM Studio: pick from the server's list; native: from the catalogue
  /model <name>                          Switch the model (next message): opus-5.5|opus-5|opus-4.8|
                                         fable-5.1|fable-5, gemini-3.1-pro|gemini-4-argon,
                                         a model the Ollama / LM Studio server lists,
                                         or a native model (experimental): qwen3.5-4b|qwen3.5-9b
  /effort                                Show the active provider's effort level and what is sent
  /effort <low|medium|high|xhigh|max>    Set effort for the active provider (Anthropic default max; Gemini high; local presets verbatim, model-validated)
  /effort <on|off>                       A native model (experimental) has no levels: think first, or answer without thinking
  /effort reset                          Clear the active provider's stored level
  /model-wait                            Show how long embraOS waits while Ollama, LM Studio or a native model sends nothing (default 120 minutes)
  /model-wait <minutes>|reset            Set that wait for the three of them, or go back to the default; Esc ends a wait
  /native                                Native models (experimental): the models' files, what is kept loaded, the model server, the job that runs
  /native install <model>                Download a model's file and check it (qwen3.5-4b|qwen3.5-9b); /native cancel stops the job that runs
  /native use <model> | off              Keep a model loaded, installing it first when needed, or free its memory; whichever provider leads
  /native context [<tokens>|reset]       The context window the loaded model is given
  /native remove <model>                 Delete a model's file
  /local-ai-gateway                      Show WSDS Local AI Gateway's URL and whether requests go through it
  /local-ai-gateway <url>|on|off|reset   Route every provider request through its unified listener (http(s)://host:4000,
                                         which must answer with its x-local-ai-gateway header); off keeps the URL
  /embeddings                            Show the semantic-similarity layer: model, index, nodes embedded
  /embeddings <on|off>                   Enable/disable semantic similarity in retrieval (default on)
  /embeddings backfill [--force]         Embed nodes that need it (local, ~55ms/node)

Media:
  /attach <id|path>              Attach an image or a text file (uploaded id, uploads/ path or any workspace path) to your next message
  /attach list | clear           Show or drop the staged attachments
  /download <path>               Hand a workspace file to your browser as a download
  /media                         Show the last image in the media pane (console-local)
  /media off | <id>              Hide the pane, or fetch a specific media id
  /image-provider                Show the image-generation backend, model, key status
  /image-provider gemini         Use Gemini image models for image_generate
  /image-provider model <id>     gemini-3-pro-image (default) | gemini-3.1-flash-image | …
  /image-provider key <token>    Set a dedicated image-generation key (STATE, 0600); `key remove` deletes it

Git & SSH:
  /git-setup <name> | <email>    Set git user config
  /github-token <token>          Set GitHub token
  /git-token <host> <token>      Set a token for a self-hosted git server (remove: /git-token <host> remove)
  /ssh-keygen                    Generate the SSH key pair (comment: the instance's name at embraos), or show the public key
  /ssh-keygen remove|rotate      Delete the key pair after showing it, or replace it with a new one
  /ssh-copy-id <user@host>       Copy SSH key to host

Guardian:
  /guardian-define               Define a dynamic tool (paste a Rust module)
  /guardian list|status <n>|show <n>|delete <n>   Manage dynamic tools
  /guardian approve <n>|reject <n>   Approve/reject a tool the intelligence proposed
  /guardian rebuild <n>|--all    Rebuild tools an OS update left unloaded, or failed builds
  /guardian key brave <token>    Set the Brave Search API key (web_search tools)
  /guardian secret <host> <header> <value>   A credential the host adds to http_request calls and websocket handshakes to that host
  /guardian egress allow|deny <host>   Private hosts the tools may reach (loopback never)
  /guardian connections [close <id>|all]   WebSocket connections the tools hold open

Identity:
  /soul                          Display the soul document
  /identity                      Display identity document
  /mode                          Show operating mode

System:
  /help                          Show this help message
  /status                        System status
  /reboot                        Say what a reboot does; /reboot now reboots embraOS through the boot menu,
                                 /reboot halt stops embraOS and powers the machine off
  /backup                        Snapshots of STATE and DATA: create [label], list, restore <name>, delete <name>, upload, status
  /ca                            Certificate authorities beside the public ones: list, import <url|uploads/file> [sha256:<fingerprint>],
                                 remove <file>; an import is trusted by the brain at once, by git and the OS updater after the next boot
  /network                       The network cards, their addresses and the machine's name: static <card> <address>/<prefix>
                                 [<gateway>] [dns <server> ...], dhcp <card>, hostname <name>|auto, time <server> ...|default|off
                                 (the time servers the clock is checked against), wifi [join <name> --key <passphrase>|
                                 join <name> --open|forget <name>|country <CC>] (blank: the card, its network and the
                                 ones in range; the passphrase is masked while typed); applied within a few seconds
  /update                        Check the published release; apply, status, rollback, source [<url>|reset];
                                 --tag <v…> names one release, apply --allow-downgrade takes an older one;
                                 auto [on|off] is the check the brain runs by itself once a day
  /web-token                     Say what the web console's access token is; /web-token rotate makes a new one:
                                 every browser is signed out, and the new URL is on the serial console and in
                                 the display console's menu
  /theme                         Show the web console's themes and the one in use; /theme <name> sets it for the whole
                                 machine: every browser, the phone's page and the display console follow, and this
                                 serial console keeps its terminal's colours; /theme reset goes back to the default
  /feedback-loop                 Trigger Phase 2 Continuity Engine self-evaluation (experimental)

Keyboard:
  Enter              Send message (or newline in /ml mode)
  Up/Down            Scroll history (PageUp/PageDown = 10 rows)
  Shift+Up/Down      Scroll the expression/reasoning panel (Shift+PageUp/PageDown = 5 rows)
  Esc                Stop the current turn"#, name)),
        _ => None,
    }
}
