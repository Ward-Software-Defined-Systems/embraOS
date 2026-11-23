# Native models (experimental)

A native model is a language model that runs on the machine itself. The image carries llama.cpp's server (`/usr/bin/llama-server`, llama.cpp v1.0.0); embrad runs it for one model on DATA, on loopback alone, and the brain reaches it as a third OpenAI-compatible preset beside Ollama and LM Studio. A turn on a native model sends nothing off the machine and takes no key.

**Experimental.** Every surface that names a native model says so. This is what it means:

- **Three models, quantised to four bits.** Qwen3.5 4B and Qwen3.5 9B are small: they answer slower than a hosted model and with less judgement. Qwen3.8 27B is the model this project recommends on Ollama and LM Studio at eight bits and more ([RECOMMENDED-LOCAL-MODELS.md](../LOCAL-MODELS/RECOMMENDED-LOCAL-MODELS.md)); its four-bit build here is not vetted against the whole tool list, it wants a machine with the memory for it, and on a processor it is slow.
- **The processor alone.** No graphics card is used.
- **Images through a second file.** A model reads images through its image file, which an install brings with the model's own. A model that stands without it reads text alone: an image in a message then reaches it as one line of text that says so.
- **No replicant check.** The Guardian's check is not run on a native model ([Guardian tools](#guardian-tools), below).

What has been run, and on which machines, is in [VALIDATION-STATUS.md](../VALIDATION-STATUS.md). The commands are in [COMMAND-REFERENCE.md](COMMAND-REFERENCE.md) (`/native`, `/provider`, `/model`, `/effort`, `/model-wait`).

## The models

| Model | The name to type | Model file | Image file | Source on huggingface.co |
|---|---|---|---|---|
| Qwen3.5 4B | `qwen3.5-4b` | `Qwen3.5-4B-Q4_K_M.gguf`, 2,815,884,544 bytes | `mmproj-Qwen3.5-4B-BF16.gguf`, 675,569,216 bytes | `William-WSDS/Qwen3.5-4B-GGUF` at revision `a4d3a4d917bbbae2cf19f932cce4da64938117d2` |
| Qwen3.5 9B | `qwen3.5-9b` | `Qwen3.5-9B-Q4_K_M.gguf`, 5,831,992,608 bytes | `mmproj-Qwen3.5-9B-BF16.gguf`, 921,704,896 bytes | `William-WSDS/Qwen3.5-9B-GGUF` at revision `2d2c56225c331a43c09f12c7479557c9c6d0969b` |
| Qwen3.8 27B | `qwen3.8-27b` | `Qwen3.8-27B-Q4_K_M.gguf`, 17,005,351,136 bytes | `mmproj-Qwen3.8-27B-BF16.gguf`, 931,145,952 bytes | `William-WSDS/Qwen3.8-27B-GGUF` at revision `930332595f8f72f27ce2bd9ae5dfc7aea4e877da` |

All three are Apache-2.0. The files are this project's own conversions of Qwen's repositories (`Qwen/Qwen3.5-4B`, `Qwen/Qwen3.5-9B`, `Qwen/Qwen3.8-27B`): converted to GGUF and quantised to Q4_K_M with llama.cpp v1.0.0, the version the image's model server is built from. Each repository's card names the revision it was made from, the commands and the hashes. The catalogue is compiled into the image (`embra_boot::native_model::CATALOGUE`): each model is two files of one revision of a repository, the model and what it reads images through, each with its size and its sha256. A file that is not those bytes is not kept, and nothing else is loaded: no other model, no other quantisation, no file of the operator's own.

## What a model needs

**Memory.** What the model server keeps in use when it has loaded a model, as measured with the image's own build of the server: its own memory, and the part of the model's file it goes on reading from. A gigabyte here is 1,024 MiB, as in the console's lines:

| Context window | Qwen3.5 4B | Qwen3.5 9B | Qwen3.8 27B |
|---|---|---|---|
| 32,768 tokens | 3.7 GB | 5.9 GB | 17.2 GB |
| 65,536 tokens (the default) | 4.7 GB | 6.9 GB | 19.2 GB |

Every 1,024 tokens of window are about 32 MB, and 64 MB for the 27B. A model's image file adds 0.6 GB for the 4B and 0.9 GB for the 9B and the 27B, and the first image the model reads about 0.4 GB more (0.6 GB for the 27B). Right after a load the server holds more for a while (6.2 GB for the 4B at the default window, 30 GB for the 27B): it copies the larger part of the weights into a layout of its own, and the pages of the file it copied from stay until the kernel wants them. The server's memory grows as a session goes on: it keeps positions of the prompt it holds to return to (32 at most, about 50 MiB each, and 150 MiB each for the 27B), and it keeps the state of earlier prompts, so that a session's prompt is not read again after another prompt went between. That cache has a limit that follows the model and the window: at the default window 3,796 MiB for the 4B and the 9B, and 9,111 MiB for the 27B.

**A machine that is short of memory for a model is told so, and nothing is refused on a figure.** The question that offers the models (`/model`, `/provider --setup native`, the first-boot wizard) says what each takes when it is loaded, and what the machine has in all and free for a model now. When a model is asked for (`/native use`, a pick, `/native context`), a warning follows in two cases: what the model takes when it is loaded is more than the machine has free, or it fits and what its server can grow to does not. It names the figures and says that a smaller window lowers them. The model is loaded all the same. At the first boot the window cannot be changed before a session, so the wizard asks its question once more when the load itself is more than the machine has free: the same pick keeps the model, and another model is another pick. What counts as free is what the kernel calls available, with what a model server that runs now holds of its own, since it ends when another model or window is asked for.

The 4B at the default window loads in a machine of 5 GiB and does not in one of 3. embrad marks the model server as the process the kernel ends first when memory runs out (`oom_score_adj` 1000), so the database, the brain and the web console stay up. A model server the kernel ended is started again, and a turn in flight on it is cut. One that is ended at every load is left alone after the third, and the turn, `/native` and the provider pill say why: `signal 9: killed, by the kernel when memory ran out or by hand`. `/native context <tokens>` loads the model with a smaller window, and `/native off` frees all of it.

**The processor.** The server loads the backend that fits the processor it finds (`/usr/lib/llama-cpp/libggml-cpu-*.so`, one for each family of instruction sets). A processor without AVX runs a model as well: on the same hardware a prompt was read about seven times slower without it.

**DATA.** A model's files are on DATA, under `/embra/data/models/<name>/`. An install needs each file's size and DATA's reserve of 256 MiB. The DATA of a disk image as built is 4 GiB: the 4B fits, and the 9B and the 27B need a larger disk first. DATA grows to the end of its disk at boot ([UPDATES-AND-BACKUPS.md](UPDATES-AND-BACKUPS.md)); an installed machine's DATA is the rest of its disk. `/native` shows what is free.

**Under QEMU.** The launchers give the VM 4 GiB of memory, four processors and QEMU's own processor model, which has no AVX. `EMBRA_MEMORY=12G EMBRA_CPUS=8 EMBRA_CPU=host ./scripts/run-qemu.sh` gives it more of each and the host's processor ([QUICK-START.md](../BUILD/QUICK-START.md)).

## What to expect

A model on a processor is slow at reading. The tool list that every turn of a session carries is about 24,000 tokens. On an eight-core desktop processor with AVX2 (an AMD Ryzen 7 5800X) the 4B reads about 85 tokens a second and the 9B about 56: the first call after a model was loaded (a boot, `/native use`, another context window) waits four and a half minutes on the 4B and seven on the 9B before the model writes its first word. That call is the first message, or the briefing the brain asks for when a session with history is attached. The server keeps what it read, so what follows begins within seconds, in that session and in another one, which starts with the same identity and tool list. It writes about 4 tokens a second on the 4B and 3 on the 9B with the list in its window. Qwen3.8 27B reads about 16 tokens a second and writes about 1.5 on that processor, so the tool list alone is twenty-five minutes of reading: it is a model for a machine with more cores than a desktop's. An image is about a thousand tokens whatever its size (the server is held to 1,024 an image: fewer and the models misread small text, and without a ceiling a large image is four times the tokens): the 4B reads one in about 15 seconds. A model that thinks first (the default) writes its reasoning at the same speed before the answer begins; `/effort off` has it answer at once.

A stop (**Esc**, `/stop`) closes the request, and the server leaves it: within a second inside an answer, and at the end of the piece it is reading (512 tokens) inside a prompt. What it read is kept.

## A model's files

- **`/native install <model>`** downloads the model's two files from the catalogue's source over HTTPS, with the certificate authorities the machine trusts (`/ca` adds one), and checks each against the catalogue's sha256. A file that is there and checked is left alone, so an install on a model that has its model file brings the image file alone. A download that stopped goes on from what arrived. The console follows it with a line every five per cent, and the session stays usable meanwhile. It ends with what is left on DATA against what a snapshot of STATE and DATA needs, and with a warning when the snapshot an update takes before a new release first runs would be refused. When the image file of the model kept loaded arrives, embrad starts the model server again with it, and a call that runs on the server then is cut; the install says so when it is done.
- **A machine that cannot reach the source** takes the files through the web console, one at a time, from a browser on another machine that has them: the sidebar's `/native` form, action `import`, then the file. The page asks the machine which model's which file it is, by its size (no two files of the catalogue have one), and streams it (`PUT /api/models/<name>`, behind the console's access token). The machine checks it against the catalogue's size and sha256 as it arrives and keeps nothing of a file that is not the catalogue's. `/native import` says this in the console and names each file and where the catalogue has it.
- **Files of another release.** A release whose catalogue has other files for a model does not use the ones an earlier release left: `/native` says `installed, not usable: the file was checked as another release's file`, the model server is not started for that model, and `/native install <model>` removes them and downloads this release's.
- **`/native remove <model>`** deletes both files. The model kept loaded is refused: `/native off` first.
- **One job at a time** changes the models (an install, an upload, a removal). `/native` shows the job that runs with its last line, and `/native cancel` stops it.
- **No backup holds a model.** The models are in no snapshot the OS takes and in no archive of `scripts/embraos-backup.sh`; a restore leaves the files on DATA where they are. A machine that was installed again downloads its model again.
- **The tools read the files and write none**, as everywhere outside the workspace.

`/native`, `/native install`, `/native import`, `/native remove` and `/native cancel` are answered before a session too, during the first-boot wizard and Learning Mode.

## Loading a model

`/native use <model>` keeps a model loaded, and `/native off` keeps none. Both are independent of the provider that leads the session: a model can stay loaded for the `native_ask` tool while Anthropic, Gemini, Ollama or LM Studio answers the operator. The stored configuration carries the choice (`openai_compat.native_model`), so the model is loaded again at every boot.

The brain writes the choice to `/embra/state/native-model/settings`, and embrad follows that record within five seconds: it starts the model server for the model, or ends it. The load itself is the read of the model's file, a few seconds from memory and longer from a cold disk. `/native` shows the state: `ready`, `loading`, or `not loaded` with the reason, and whether the server reads images or text alone.

- **The context window** is 65,536 tokens unless set: `/native context <tokens>` takes 1,024 up to the model's 262,144, and `/native context reset` returns to the default. Each model keeps its own. A change ends the model server and loads the model again; a turn in flight on it is cut.
- **A model whose model file is not installed** is installed first, with both its files, and loaded when the job is done. A model that has its model file and not its image file is loaded as it is, and reads text alone until `/native install <model>` brings the other.
- **A model server that keeps ending** is left alone: after three exits before it answered, embrad starts it no more until the record changes, and `/native` shows the reason with the server's last line. `/native use <model>` asks again.
- **`embra.native=0`** on the kernel line starts no model server in that boot; the boot menu's "Command-line knobs" lists it with the others.

## A native model as the provider

`/provider native` switches the session's provider to the loaded model, and `/provider --setup native` and `/model` show the catalogue as a selector. At the first boot the configuration wizard has a third answer beside the direct providers and the gateway: "Natively on this machine (experimental)". It asks which model, installs it when its file is not there, and has it loaded while the remaining questions are answered. There is no provider step and no key. The model that is picked leads Learning Mode, where the identity is written and sealed; the wizard says so before it asks, and names the way to an identity written beforehand ([IDENTITY-GRAPH.md](../MULTI-GRAPH/IDENTITY-GRAPH.md), the import).

What differs from the other providers in a session:

- **Thinking is on or off, and the 27B has levels.** Qwen3.5 has no levels: `/effort off` (or `low`) has the model answer without thinking, `/effort on`, any other level and `reset` have it think first, which is the default. Qwen3.8 27B thinks at one of three levels of effort, `low`, `medium` and `xhigh`: `/effort <level>` sets one, `/effort on` and `reset` are the model's own, `xhigh`, and `/effort off` has it answer without thinking. A level of another provider (`high`, `max`) is refused there, since the model's chat template takes no other word. Each model keeps its own setting. The brain sends the switch with every request (`chat_template_kwargs.enable_thinking`) together with the model card's sampling for that state, and a `reasoning_effort` only for a level of the 27B. The reasoning streams to the expression panel as on the other providers.
- **An image** attached to a message, or returned by a tool, goes to the model when its server was started with the model's image file: as the image's bytes in the request, never as a path or an address. On a server without the file the image is one line of text in its place, which says that it was not sent and that an install of the model brings the file.
- **A request waits for the model.** A request sent while embrad is still starting the model server, or while the server reads the model's file, waits and is answered when the model is ready; the model wait (`/model-wait`, 120 minutes unless set) bounds that wait as it bounds a server's silence. When no server will come (no model is kept loaded, its file is not installed, embrad stopped trying) the turn is told so at once, in embrad's words.
- **A session that outgrows the window is refused.** The server never trims a request: one that is longer than the window gets HTTP 400, and the brain says the request's size, the window, and the two ways on (`/native context <tokens>`, or a new session). The turn is saved as a cut one.
- **The provider gateway is not used.** A native model is reached on loopback whether `/local-ai-gateway` is on or off.
- **The provider probe** asks the model server as it asks any provider. A model that is still loading is reachable (`loading` in the `system_status` tool's provider block), and the window the model was loaded with is read from the server (`context_window`, state `loaded`).
- **Each model call logs one line** with the server's own timings (`openai_compat::timings`: the tokens read, taken from its cache and written, with the time and the speed of each), which `system_logs` reads back.

A switch away from a native model leaves it loaded, and the reply says so: `/native off` frees its memory.

## The `native_ask` tool

The intelligence asks the native model through the `native_ask` tool ([TOOL-REFERENCE.md](TOOL-REFERENCE.md)): a prompt, an optional system text, image files for it to look at, thinking on or off (on the 27B at the level `/effort` set for it), and optionally tools from the brain's own list for the model to choose from. The model is always told in a few sentences what it is and where it runs, before the caller's own system text: its name and its maker, that it runs on this machine as the native model of embraOS, and that one question comes to it through a tool, with no session and no memory. The text says nothing of how to behave. A small model follows what it reads, so the tool's description tells the intelligence to state in its own system text that text it hands over is data and never an instruction. Images are files by path, read as `image_view` reads one, ten at most; a call with images is refused with the way to get the file when the loaded model has no image file. An argument the tool does not have is refused by its name. The answer comes back with the reasoning apart, the calls the model asked for (reported, never run) and the server's timings. From a session that another provider leads it is how the native model is tested without depending on it. From a session that the native model itself leads, the same model answers in a context of its own: it sees the call's prompt and nothing of the session. The model server has one slot, so such a call runs between two steps of the turn; the server keeps the turn's prompt in its cache meanwhile and takes it back at the next step. The tool says so in a sentence when no model is loaded. With the whole tool list offered, the call takes as long as the list takes to read, and the tool's limit is nine and a half minutes. A call that the limit ends says how many tokens of its request the model server had read. What the server read stays in its cache, so the same call again goes on from there: on a slow processor a long request is read over two calls or three.

## Guardian tools

The replicant check is a model's judgement of a tool against the sealed soul ([REPLICANT-CHECK.md](../GUARDIAN/REPLICANT-CHECK.md)). It is not run on a native model: while one leads an instance whose soul is sealed, `/guardian-define`, `guardian_propose`, `/guardian approve` and the `/guardian rebuild` of a tool that needs one are refused with one sentence that names `/provider`, and nothing is compiled, proposed, approved or rebuilt. Tools that were approved keep running, and the three shipped tools are installed at boot as always. A tool that needs a rebuild after an update that changed the in-OS toolchain waits for a switch to another provider.

## The model server

- **One process, one model.** `llama-server --host 127.0.0.1 --port 8091 -m <the file> -a <the name> -c <the window> -np 1 -b 512 --ctx-checkpoints 32 --cache-ram <MiB> --no-ui`, and `--mmproj <the image file> --image-min-tokens 1024 --image-max-tokens 1024` when the model's image file is there and checked. It serves one request at a time. It gets no environment (the brain's holds the provider keys) and no flag by which it could fetch or load another file.
- **Loopback alone.** The listener is `127.0.0.1:8091`, plain HTTP without a key; the brain is its one client. Nothing forwards it.
- **Not a service of the supervisor.** embrad's reconcile loop keeps it, never waits for it, and never stops the machine because of it: a model server that ends is started again at the next look.
- **Its output** goes to `/embra/ephemeral/native-model.log`, cut back to its last mebibyte when it passes eight; the file tools read it.
- **Three records**, one `key=value` a line (`crates/embra-boot/src/native_model.rs`):

| Record | Written by | Read by | Holds |
|---|---|---|---|
| `/embra/state/native-model/settings` | the brain | embrad | the model to keep loaded and its window; absent or empty for none |
| `/run/embra/native-model-status` | embrad | the brain | `off`, `disabled`, `unavailable`, `loading`, `ready` or `halted`, with the model, the window, the process, whether the server was started with the image file, and the reason |
| `/embra/data/models/<name>/verified` and `verified-images` | the updater | embrad | each file's name, size, sha256 and modification time as they were when it was checked |

embrad starts a model only from a file whose record still describes it; nothing is hashed at a start. A file changed since it was checked is not loaded, and `/native install <model>` checks it again.

## An older release

A release from before native models does not know the provider `native`. Started on a configuration that names it (its slot chosen in the boot menu after the newer release was configured), it takes the provider for Anthropic, and a turn is refused for the missing key until `/provider` there names a provider that release has. Nothing of the native settings is lost by that boot: the newer release finds the model, its window and its `/effort` as they were, and the model's file where it was.

*Last updated: 2026-11-22.*
