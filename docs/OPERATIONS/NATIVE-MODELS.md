# Native models (experimental)

A native model is a language model that runs on the machine itself. The image carries llama.cpp's server (`/usr/bin/llama-server`, llama.cpp v1.0.0); embrad runs it for one model file on DATA, on loopback alone, and the brain reaches it as a third OpenAI-compatible preset beside Ollama and LM Studio. A turn on a native model sends nothing off the machine and takes no key.

**Experimental.** Every surface that names a native model says so. This is what it means:

- **Two small models.** Qwen3.5 4B and Qwen3.5 9B, quantised to four bits. They answer slower than a hosted model and with less judgement. The models vetted against the whole tool list are larger ([RECOMMENDED-LOCAL-MODELS.md](../LOCAL-MODELS/RECOMMENDED-LOCAL-MODELS.md)).
- **The processor alone.** No graphics card is used.
- **Text only.** An image in a message reaches the model as one line of text that says an image was attached.
- **No replicant check.** The Guardian's check is not run on a native model ([Guardian tools](#guardian-tools), below).

What has been run, and on which machines, is in [VALIDATION-STATUS.md](../VALIDATION-STATUS.md). The commands are in [COMMAND-REFERENCE.md](COMMAND-REFERENCE.md) (`/native`, `/provider`, `/model`, `/effort`, `/model-wait`).

## The models

| Model | The name to type | File | Size | Source on huggingface.co |
|---|---|---|---|---|
| Qwen3.5 4B | `qwen3.5-4b` | `Qwen3.5-4B-Q4_K_M.gguf` | 2,740,937,888 bytes | `unsloth/Qwen3.5-4B-GGUF` at revision `e87f176479d0855a907a41277aca2f8ee7a09523` |
| Qwen3.5 9B | `qwen3.5-9b` | `Qwen3.5-9B-Q4_K_M.gguf` | 5,680,522,464 bytes | `unsloth/Qwen3.5-9B-GGUF` at revision `3885219b6810b007914f3a7950a8d1b469d598a5` |

Both are Apache-2.0. The catalogue is compiled into the image (`embra_boot::native_model::CATALOGUE`): each model is one file of one revision of a repository, with its size and its sha256. A file that is not those bytes is not kept, and nothing else is loaded: no other model, no other quantisation, no file of the operator's own.

## What a model needs

**Memory.** The model server's memory with a model loaded and a prompt read, as measured:

| Context window | Qwen3.5 4B | Qwen3.5 9B |
|---|---|---|
| 32,768 tokens | 5.2 GB | 9.4 GB |
| 65,536 tokens (the default) | 6.3 GB | 10.5 GB |

Each further 32,768 tokens of window take about 1.05 GB. The server's memory grows from there as a session goes on: it keeps positions of the prompt it holds to return to (32 at most, about 50 MiB each), and it keeps the state of earlier prompts, so that a session's prompt is not read again after another prompt went between. That cache has a limit that follows the window: 3,796 MiB at the default window.

Nothing checks the machine's memory before a model is loaded: the 4B at the default window loads in 5 GiB and does not in 3. embrad marks the model server as the process the kernel ends first when memory runs out (`oom_score_adj` 1000), so the database, the brain and the web console stay up. A model server the kernel ended is started again, and a turn in flight on it is cut. One that is ended at every load is left alone after the third, and the turn, `/native` and the provider pill say why: `signal 9: killed, by the kernel when memory ran out or by hand`. `/native context <tokens>` loads the model with a smaller window, and `/native off` frees all of it.

**The processor.** The server loads the backend that fits the processor it finds (`/usr/lib/llama-cpp/libggml-cpu-*.so`, one for each family of instruction sets). A processor without AVX runs a model as well: on the same hardware a prompt was read about seven times slower without it.

**DATA.** A model's file is on DATA, under `/embra/data/models/<name>/`. An install needs the file's size and DATA's reserve of 256 MiB. The DATA of a disk image as built is 4 GiB: the 4B fits, and the 9B needs a larger disk first. DATA grows to the end of its disk at boot ([UPDATES-AND-BACKUPS.md](UPDATES-AND-BACKUPS.md)); an installed machine's DATA is the rest of its disk. `/native` shows what is free.

**Under QEMU.** The launchers give the VM 4 GiB of memory, four processors and QEMU's own processor model, which has no AVX. `EMBRA_MEMORY=12G EMBRA_CPUS=8 EMBRA_CPU=host ./scripts/run-qemu.sh` gives it more of each and the host's processor ([QUICK-START.md](../BUILD/QUICK-START.md)).

## What to expect

A model on a processor is slow at reading. The tool list that every turn of a session carries is about 24,000 tokens. On an eight-core desktop processor with AVX2 (an AMD Ryzen 7 5800X) the 4B reads about 85 tokens a second and the 9B about 56: the first call after a model was loaded (a boot, `/native use`, another context window) waits four and a half minutes on the 4B and seven on the 9B before the model writes its first word. That call is the first message, or the briefing the brain asks for when a session with history is attached. The server keeps what it read, so what follows begins within seconds, in that session and in another one, which starts with the same identity and tool list. It writes about 4 tokens a second on the 4B and 3 on the 9B with the list in its window. A model that thinks first (the default) writes its reasoning at the same speed before the answer begins; `/effort off` has it answer at once.

A stop (**Esc**, `/stop`) closes the request, and the server leaves it: within a second inside an answer, and at the end of the piece it is reading (512 tokens) inside a prompt. What it read is kept.

## A model's file

- **`/native install <model>`** downloads the file from the catalogue's source over HTTPS, with the certificate authorities the machine trusts (`/ca` adds one), and checks it against the catalogue's sha256. A download that stopped goes on from what arrived. The console follows it with a line every five per cent, and the session stays usable meanwhile. It ends with what is left on DATA against what a snapshot of STATE and DATA needs, and with a warning when the snapshot an update takes before a new release first runs would be refused.
- **A machine that cannot reach the source** takes the file through the web console, from a browser on another machine that has it: the sidebar's `/native` form, action `import`, then the file. The page asks the machine which model the file is, by its name or else its size, and streams it (`PUT /api/models/<name>`, behind the console's access token). The machine checks it against the catalogue's size and sha256 as it arrives and keeps nothing of a file that is not the catalogue's. `/native import` says this in the console and names each file and where the catalogue has it.
- **`/native remove <model>`** deletes the file. The model kept loaded is refused: `/native off` first.
- **One job at a time** changes the models (an install, an upload, a removal). `/native` shows the job that runs with its last line, and `/native cancel` stops it.
- **No backup holds a model.** The models are in no snapshot the OS takes and in no archive of `scripts/embraos-backup.sh`; a restore leaves the files on DATA where they are. A machine that was installed again downloads its model again.
- **The tools read the files and write none**, as everywhere outside the workspace.

`/native`, `/native install`, `/native import`, `/native remove` and `/native cancel` are answered before a session too, during the first-boot wizard and Learning Mode.

## Loading a model

`/native use <model>` keeps a model loaded, and `/native off` keeps none. Both are independent of the provider that leads the session: a model can stay loaded for the `native_ask` tool while Anthropic, Gemini, Ollama or LM Studio answers the operator. The stored configuration carries the choice (`openai_compat.native_model`), so the model is loaded again at every boot.

The brain writes the choice to `/embra/state/native-model/settings`, and embrad follows that record within five seconds: it starts the model server for the model, or ends it. The load itself is the read of the model's file, a few seconds from memory and longer from a cold disk. `/native` shows the state: `ready`, `loading`, or `not loaded` with the reason.

- **The context window** is 65,536 tokens unless set: `/native context <tokens>` takes 1,024 up to the model's 262,144, and `/native context reset` returns to the default. Each model keeps its own. A change ends the model server and loads the model again; a turn in flight on it is cut.
- **A model whose file is not installed** is installed first, and loaded when the job is done.
- **A model server that keeps ending** is left alone: after three exits before it answered, embrad starts it no more until the record changes, and `/native` shows the reason with the server's last line. `/native use <model>` asks again.
- **`embra.native=0`** on the kernel line starts no model server in that boot; the boot menu's "Command-line knobs" lists it with the others.

## A native model as the provider

`/provider native` switches the session's provider to the loaded model, and `/provider --setup native` and `/model` show the catalogue as a selector. At the first boot the configuration wizard has a third answer beside the direct providers and the gateway: "Natively on this machine (experimental)". It asks which model, installs it when its file is not there, and has it loaded while the remaining questions are answered. There is no provider step and no key. The model that is picked leads Learning Mode, where the identity is written and sealed; the wizard says so before it asks, and names the way to an identity written beforehand ([IDENTITY-GRAPH.md](../MULTI-GRAPH/IDENTITY-GRAPH.md), the import).

What differs from the other providers in a session:

- **Thinking is on or off.** Qwen3.5 has no levels: `/effort off` (or `low`) has the model answer without thinking, `/effort on`, any other level and `reset` have it think first, which is the default. The brain sends the switch with every request (`chat_template_kwargs.enable_thinking`) together with the model card's sampling for that state, and no `reasoning_effort`. The reasoning streams to the expression panel as on the other providers.
- **A request waits for the model.** A request sent while embrad is still starting the model server, or while the server reads the model's file, waits and is answered when the model is ready; the model wait (`/model-wait`, 120 minutes unless set) bounds that wait as it bounds a server's silence. When no server will come (no model is kept loaded, its file is not installed, embrad stopped trying) the turn is told so at once, in embrad's words.
- **A session that outgrows the window is refused.** The server never trims a request: one that is longer than the window gets HTTP 400, and the brain says the request's size, the window, and the two ways on (`/native context <tokens>`, or a new session). The turn is saved as a cut one.
- **The provider gateway is not used.** A native model is reached on loopback whether `/local-ai-gateway` is on or off.
- **The provider probe** asks the model server as it asks any provider. A model that is still loading is reachable (`loading` in the `system_status` tool's provider block), and the window the model was loaded with is read from the server (`context_window`, state `loaded`).
- **Each model call logs one line** with the server's own timings (`openai_compat::timings`: the tokens read, taken from its cache and written, with the time and the speed of each), which `system_logs` reads back.

A switch away from a native model leaves it loaded, and the reply says so: `/native off` frees its memory.

## The `native_ask` tool

The intelligence asks the native model through the `native_ask` tool ([TOOL-REFERENCE.md](TOOL-REFERENCE.md)): a prompt, an optional system text, thinking on or off, and optionally tools from the brain's own list for the model to choose from. The answer comes back with the reasoning apart, the calls the model asked for (reported, never run) and the server's timings. From a session that another provider leads it is how the native model is tested without depending on it. From a session that the native model itself leads, the same model answers in a context of its own: it sees the call's prompt and nothing of the session. The model server has one slot, so such a call runs between two steps of the turn; the server keeps the turn's prompt in its cache meanwhile and takes it back at the next step. The tool says so in a sentence when no model is loaded. With the whole tool list offered, the call takes as long as the list takes to read, and the tool's limit is nine and a half minutes.

## Guardian tools

The replicant check is a model's judgement of a tool against the sealed soul ([REPLICANT-CHECK.md](../GUARDIAN/REPLICANT-CHECK.md)). It is not run on a native model: while one leads an instance whose soul is sealed, `/guardian-define`, `guardian_propose`, `/guardian approve` and the `/guardian rebuild` of a tool that needs one are refused with one sentence that names `/provider`, and nothing is compiled, proposed, approved or rebuilt. Tools that were approved keep running, and the three shipped tools are installed at boot as always. A tool that needs a rebuild after an update that changed the in-OS toolchain waits for a switch to another provider.

## The model server

- **One process, one model.** `llama-server --host 127.0.0.1 --port 8091 -m <the file> -a <the name> -c <the window> -np 1 -b 512 --ctx-checkpoints 32 --cache-ram <MiB> --no-ui`. It serves one request at a time. It gets no environment (the brain's holds the provider keys) and no flag by which it could fetch or load another file.
- **Loopback alone.** The listener is `127.0.0.1:8091`, plain HTTP without a key; the brain is its one client. Nothing forwards it.
- **Not a service of the supervisor.** embrad's reconcile loop keeps it, never waits for it, and never stops the machine because of it: a model server that ends is started again at the next look.
- **Its output** goes to `/embra/ephemeral/native-model.log`, cut back to its last mebibyte when it passes eight; the file tools read it.
- **Three records**, one `key=value` a line (`crates/embra-boot/src/native_model.rs`):

| Record | Written by | Read by | Holds |
|---|---|---|---|
| `/embra/state/native-model/settings` | the brain | embrad | the model to keep loaded and its window; absent or empty for none |
| `/run/embra/native-model-status` | embrad | the brain | `off`, `disabled`, `unavailable`, `loading`, `ready` or `halted`, with the model, the window, the process, and the reason |
| `/embra/data/models/<name>/verified` | the updater | embrad | the file's name, size, sha256 and modification time as they were when it was checked |

embrad starts a model only from a file whose record still describes it; nothing is hashed at a start. A file changed since it was checked is not loaded, and `/native install <model>` checks it again.

## An older release

A release from before native models does not know the provider `native`. Started on a configuration that names it (its slot chosen in the boot menu after the newer release was configured), it takes the provider for Anthropic, and a turn is refused for the missing key until `/provider` there names a provider that release has. Nothing of the native settings is lost by that boot: the newer release finds the model, its window and its `/effort` as they were, and the model's file where it was.

*Last updated: 2026-11-22.*
