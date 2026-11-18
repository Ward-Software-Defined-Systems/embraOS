# embraOS — Open Problems

Unresolved design questions tracked at the architecture level rather than as code comments. Each is something that will need a decision during Phase 2–3 implementation. Implementation bugs go in the Embra_Debug tracker — this list is design-state tracking, not defects.

Each entry states the question, what exists today and what is still open. How an entry came to its present state is in the [change log](CHANGE-LOG.md).

---

## Module trust escalation

Operator-authored modules start sandboxed. How do they earn broader access? A trust ladder is needed: sandbox → internal-only → governed-egress → full-egress. Each step requires governance approval plus operational history. The ladder design is not yet specified.

**What exists**, for dynamic tools (`embra-guardian`). A tool runs in a `wasmtime` sandbox with no ambient authority and reaches out only through the host imports it declares: `http_get` (https, public addresses only), `web_search`, `http_request` and `websocket` (`KNOWN_CAPS` in `crates/embra-guardian/src/abi.rs`; the rules in `caps.rs`). A declaration is granted when the tool passes the replicant check and, for a proposal of the intelligence, the operator's `/guardian approve`; the three tools the image ships hold their declarations by the project's review, at install. An internal-only rung is the operator's: `/guardian egress allow <host>` admits a private host (a name with its subdomains, or an IP literal) to the `http_get` and `http_request` guards, and a loopback, link-local, multicast or reserved address is refused whatever the list says (`AddressClass` in `caps.rs`). A credential for a host is stored with `/guardian secret` and injected by the host on a matching request only, so a tool holds no secret. `websocket` is the one grant that outlives a call: a `wss://` connection the brain holds for the tool between calls (`guarded_websocket` in `crates/embra-guardian/src/ws.rs`; the connections in `crates/embra-brain/src/guardian/ws.rs`). It is opened under the rules of `http_request` — the address classes, the allowlist, the host's stored secret on the handshake — and what bounds it afterwards is fixed: four connections a tool and eight in all, ten minutes unused, sixty minutes at most, a close when its host is denied or its secret removed, and the operator's `/guardian connections`.

**Still open.** All of it is approval per definition: nothing is earned from operating history, and the ladder is not specified.

## Resource contention: LLM vs modules

Local LLM inference is resource-intensive. Module containers also need CPU and memory. embrad needs to arbitrate. The Continuity Engine should reason about "more inference capacity" vs "more module capacity" as a scheduling decision — but the scheduling policy is not yet designed.

## Governance latency

Every governed operation goes through embra-guardian. If governance evaluation involves LLM reasoning, this adds latency to the hot path. Proposed dual-path: deterministic rule-engine for hot-path governance (fast), LLM-based evaluation for complex or novel requests (slow but thorough). The threshold between paths is not yet defined.

**What exists.** Dynamic tools use the dual path, split by time instead of by request. The evaluation that needs a model, the replicant check, runs when a tool is defined, proposed, approved or rebuilt (`run_replicant_check` in `crates/embra-brain/src/guardian/mod.rs`) and never when it is called. A call meets deterministic limits only: the sandbox's epoch timeout and memory cap, and the capability broker's rules.

**Still open.** The threshold between the paths for an operation that is governed at the moment it runs, as a proxied MCP call would be.

## WardSONDB as single point of failure

WardSONDB is a core OS service. If it fails, the brain can't read state and the feedback loop halts. Mitigations: WAL-based crash recovery (fjall's built-in durability), read replica for continuity during recovery, snapshot-based restore as last resort. The replica architecture is not yet designed.

**What exists.** Two of the three mitigations. Crash recovery is the storage engine's — fjall or rocksdb, chosen when the image is built (`--storage-engine`) — and `embrad` restarts the database with backoff under a burst budget (`crates/embrad/src/supervisor.rs`). Snapshots are the OS's own: `/backup` on a running machine and the boot menu's Restore entry ([UPDATES-AND-BACKUPS.md](OPERATIONS/UPDATES-AND-BACKUPS.md)), with `scripts/embraos-backup.sh` from the host for an image that is not running. While the database is away the brain stays up and says so: its client has a connect timeout and a read-idle timeout, and a read that fails is reported as a failure — an Error frame at a turn, an attach or a `/switch`, a tool reply that says the read failed — never as empty state.

**Still open.** There is no replica, and no design for one.

## Bare metal vs K8s isolation parity

In bare metal mode, module containers share the same kernel as embraOS. In K8s mode, modules run in a separate namespace with network policies. Bare metal needs stronger containerd-level isolation (seccomp, AppArmor/SELinux profiles, user namespaces) to match K8s-level isolation. The seccomp/AppArmor profiles are not yet written.

## Module image provenance

If module source originates inside the OS (operator-authored via Guardian, or — under future governance design — brain-proposed), the provenance chain must be auditable end-to-end: source code → `modules.source` → sandboxed build → image signing → governance review → allowlist → deploy. Each step must be logged and verifiable. The sandboxed build pipeline is not yet designed.

**What exists**, for dynamic tools. The record of a tool (`ToolDoc` in `crates/embra-guardian/src/store.rs`, collection `guardian.tools`) keeps its source and the SHA-256 of it, the verdict of the replicant check with the model that judged and the time, the toolchain version it was built with, the tail of the build log, and the hash of the artifact the build loaded: boot reconcile loads an artifact only when the file on DATA hashes to it (`artifact_check` in `crates/embra-brain/src/guardian/mod.rs`). A tool has no third-party dependency, so its build runs no code but the compiler's. `/guardian approve` builds a proposal only when its stored source still matches the hash recorded with it, and `/guardian show` prints that digest, so the approval is bound to the source the operator saw.

**Still open.** Not kept: a record of the operator's approval (building a proposal is the approval), a signature of the built artifact, and a log of each step that can be verified afterwards. Modules as images, with signing and an allowlist, are not designed.

## Does the auto-derived edge layer still earn its cost?

`same_session`, `temporal` and `tag_overlap` are nearly all of a graph's edges: 430,271 of 433,325 on a production graph (99.3%). They grow about 1.7 times faster than the node layer (edges +44.5% against nodes +26% over 19 days), and 21% of sampled nodes exceed the 500-edge auto window.

That layer's headline justification was per-turn depth-2 expansion during retrieval: density was the substrate expansion needed to find adjacent nodes. Measurement retired that argument, and retrieval does no expansion now. Expansion reached a 1,000-node slab (42% of the graph) that was 97.7–99.6% auto-reached, cost ~96% of retrieval latency, and contributed **nothing** to the injected top-20 on any query tested.

What remains is `knowledge_traverse`, which the intelligence invokes deliberately rather than on every turn — and the control case is instructive: identity nodes carry no auto edges, and a traversal from one is **100% meaningful**. The graph is useful exactly where the auto layer does not drown it.

`remember` promotes a memory when it is created, so every memory derives twice: once for its entry and once for its node, which carries the same text, tags and session.

Options, none taken: decay `same_session`/`temporal` weights with age and prune below a floor; stop double-writing auto edges; derive for the node alone, so that a memory adds about half of what it adds today, at a price in retrieval, whose session step walks `same_session` edges from the session's entries (`retrieval.rs`, step 2) and would have to be examined again; cap per-node auto degree at write time; or accept the growth as the cost of a traversal tool used a few times a session. The decision needs a measurement of what `knowledge_traverse` actually returns in practice, which does not exist yet. Related: the WardSONDB traverse endpoint stays deferred precisely because the step it would accelerate was the one deleted (the measurements are in [KNOWLEDGE-GRAPH.md](MULTI-GRAPH/KNOWLEDGE-GRAPH.md), under retrieval).

## The web console's certificate: what it does not name

The certificate `embra-trustd` mints for `embra-web` names the machine as its network sees it: `localhost`, the machine's name, the loopback addresses and every private address a card holds (`embra_boot::names::leaf_names`). It is minted again when those change, and the certificate of the CA that signs it is handed out at `/api/ca.crt` for a browser to import once ([QUICK-START.md](BUILD/QUICK-START.md), The web console's certificate). Four things stay outside it.

- **A name with a domain.** A browser pointed at `axiom.lan`, the name a router's DNS serves for the machine, is shown a certificate that names `axiom`. The machine's domain is the network's word (the lease's), not its own, and the CA's name constraints admit the names it was made with. mDNS (`embraos.local`) is deferred with the roadmap.
- **A name a DHCP server assigns.** It is the machine's name and is not on the certificate: the CA would be issued again each time a network renamed the machine. `/network hostname <name>` makes a name the machine's own and the certificate's.
- **A public address.** The CA signs for loopback, the private ranges and the link-local range alone, so a machine reached at a public address shows a certificate that does not name it. The limit is meant: the key in STATE cannot sign for a public name or address, whoever holds it.
- **The import is by hand.** A browser trusts the CA after its operator imports the file, and after `/network hostname` issues the CA again, once more: a copy from before keeps the limits it had.

## The clock: an unauthenticated answer

embrad sets the clock at boot and checks it while the OS runs: a hardware clock that reads earlier than the image's build time is moved forward (to the last time the machine saved, or to the build time), and a time server is asked by SNTP, before any service starts and every hour after (`crates/embrad/src/clock.rs`; [QUICK-START.md](BUILD/QUICK-START.md), The clock). Three things are open.

- **The answer is not authenticated.** embrad ties an answer to its request with a random value the server returns, which stops a forger off the path, and refuses a time earlier than the image's build time. A host on the path can still forge a later time. On a running machine one bound holds: once a time server has set the clock in a boot, an answer more than fifteen minutes away from it is recorded and not applied. A boot's first answer is taken whole. NTS (RFC 8915) would authenticate the answer; it is not implemented.
- **The source.** The default is one name, `time.cloudflare.com` (`DEFAULT_TIME_SERVER`), asked after the time servers a DHCP lease names; the operator's own (`/network time`) replace both. The NTP Pool's zone names are not used: its rules for vendors forbid them as the default of a distributed system without a vendor zone, and embraOS has none; with one, the default becomes that zone. A DHCP server is believed about its time servers as it is about its router: a host that answers DHCP on a network is on the path already.
- **A board without a clock battery.** A board whose clock battery is dead loses its time whenever power is removed: after that it has the right time once a time server answers, at boot or at the check that follows the network's arrival. Without one it resumes from the last time it saved (the clock, written to STATE once a minute), behind by as long as the power was off: the clock never restarts before where it stopped, and it is not right until a time server answers.

## An interactive console on the display

The display is a boot console: GRUB's menu, the boot log, the banner with the console URL and the reason a boot stopped all appear on it, so a hypervisor or a bare-metal machine without a serial port shows what happened. A machine without a serial port has a `/dev/console` that opens and refuses every write; nothing in the boot stops on it.

**The maintenance boots answer on either console.** `embra-update` speaks and listens through one operator channel (`console::Operator`), which under `--console` is `BootConsole::prompt_line` over `/dev/console` and `/dev/tty0` at once: the prompt on both, the first line typed on either answering, a console that refuses to be read or written skipped. embrad passes `--console` to every job it runs before the services (the Update, Backup and Restore entries, the installer, the trial and scheduled snapshots, the scheduled restore, DATA's growth).

**The running system has the display console.** After the banner, embrad starts eudev and cog (WPE WebKit's DRM platform) on the display, which loads the web console with the access token, keyboard and mouse through libinput, the session shared with every browser under the existing arbiter. It renders in software end to end (Mesa's kms_swrast and wayland-swrast, cog's dumb-buffer scanout), because displays this image meets (the firmware's framebuffer, QEMU's standard VGA, Hyper-V's video) have neither a DRM render node nor a cursor plane, and a card that has both is drawn on the same way. embrad takes a display's modes from sysfs, where a connector's list is the one its last reader left; when the mode it asks for is missing it writes `detect` to each connector's `status` and reads once more (never on a kernel line whose `video=` option forces a connector). On Parallels Desktop the display console follows the display adapter the machine's type brings: a machine typed Linux has Parallels' virtio adapter and comes up at 1920x1080, one typed "Other" has Parallels' own adapter and stays at the firmware's 1024x768.

**It rests on six patches the build carries** (`buildroot/patches/`, one on Mesa and five on cog), to be carried upstream. Three of cog's are what an operator would meet without them. `0003-drm-pointer-buttons-in-WPE-s-numbering-the-held-ones-as-modifiers.patch`: cog's DRM platform handed WPE the button as libinput names it, the evdev code, while WebKit knows the buttons as 1, 2 and 3, so a press arrived as a mouse event without a button and nothing was clicked. `0004-drm-a-card-libdrm-does-not-enumerate-is-opened-by-its-node.patch`: libdrm tells a card by the bus of the device behind it and leaves out every other one; Hyper-V's synthetic video is a VMBus device, so cog found no display there. `0005-drm-a-held-key-is-pressed-once-in-the-xkb-state-a-modifier-does-not-repeat.patch`: cog's DRM platform repeats a held key itself and gave every repeat to xkbcommon as one more press of the key, while xkbcommon clears a modifier when its releases have matched its presses, so a Shift, Ctrl or Alt held for longer than half a second stayed set after its release.

**What the launcher does not draw, the page draws.** WPE WebKit's theme paints a light button of its own over a styled `<select>` (`MenuListAdwaita::draw` for the styled box too) and keeps the page's text on it. cog draws a `<select>`'s list on its Wayland platform alone (`platform/wayland/cog-popup-menu-wl.c`), so on the DRM platform a click opened nothing; the engine took its list for open from then on (`RenderMenuList::m_popupIsVisible`, cleared by the launcher's answer or when the box loses the focus) and no longer wrote the choice into the box while the arrow keys went on changing it: a form could run a choice its box did not show. cog has no file picker and no dialog for a page's alert either (WPE's defaults decline both without a word). The page covers all of it on the display and nowhere else, as it draws the pointer and the menu: the box is the page's field with an arrow drawn there, the list is the page's under the box for the browser's own element (the engine's answer to a press, to Space and to Enter on the box never runs, so its list is never taken for open), and one sheet says what the display cannot finish, with this console's URL: the Attach button, a backup's upload, a click on a download row, a page's alert. A browser on another machine runs none of it: the page script's effects outside the display console were compared with the former script's, one for one. A list drawn by the launcher (one more patch on cog's DRM platform) was not taken: it is the larger change, and cog's upstream has no such code to meet.

**Still open.**

- The serial TUI (`embra.tui=1`) draws and reads on the serial line alone, so a machine with a display and no serial port has the display console and not the TUI. An option, not taken: run `embra-console` on `/dev/tty1` as well when the kernel has a framebuffer (the kernel carries the keyboard drivers; the TUI would need the Linux console's terminal type and no serial graphics protocol).
- The display console loads the loopback page with TLS errors ignored; trusting embra-trustd's CA through GIO instead is the follow-up.
- A Compose table for xkbcommon (the kernel of a dead-key layout) is not shipped.
- The six patches are carried, not upstream.
