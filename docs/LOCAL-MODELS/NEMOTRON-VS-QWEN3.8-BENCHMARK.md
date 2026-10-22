# Nemotron 3.5 Super vs Qwen3.8 27B on the Mac Studio

Long-context benchmark of local models for embraOS on a Mac Studio, in four parts:
- **Nemotron 3.5 Super vs Qwen3.8 27B** on Ollama's MLX runner, both at 4-bit NVFP4.
- **Qwen3.8 at 4, 8 and 16 bits** on Ollama.
- **Qwen3.8 8-bit and 16-bit on LM Studio.**
- **A repeat test** on how long Qwen3.8 reasons in each setup.

Every configuration ran twice, once with thinking off and once at the highest reasoning level the model offers, which is what embraOS's max-reasoning presets will send.

*Measured 2026-10-21 and 22 · Mac Studio M4 Max, 128 GB · Ollama 0.40.6 · LM Studio 0.4.12 with MLX runtime 1.13.1 · identical documents throughout*

## Summary

**Nemotron vs Qwen3.8 (both 4-bit, Ollama)**
- **Nemotron is faster at every size, and the gap grows with context.**
  - Reading the prompt: 2.0× faster at 32K, 2.2× at 128K, 2.6× at 256K.
  - Generating with thinking on: 1.2×, 1.5× and 1.8× faster.
- **Max reasoning added little time on this task.** From 128K up, most of the wait was reading the prompt. Complete answers, Nemotron vs Qwen3.8:

  | Context | Nemotron | Qwen3.8 |
  |---|---|---|
  | 32K | 2.7 min | 3.8 min |
  | 128K | 7.0 min | 14.4 min |
  | 256K | 16.6 min | 40.8 min |
- **Both models read the whole document.** Each named every module in the 32K and 128K documents and 41 of the 42 in the 256K document. Neither named a module that was not there.
- **Qwen3.8's 262,144-token window holds the prompt, the thinking and the answer.** The 256K document left about 14,500 tokens for thinking and answer. Nemotron's 1M window (about 850K usable in this Mac's memory) leaves room.
- **The quality signals favor Qwen3.8.** On Artificial Analysis it scores 34 vs 26 overall, 82% vs 69% on long-context reasoning, and 48% vs 15% on agentic AutomationBench. It is embraOS's vetted pick. Nemotron, with 12B active parameters, is below embraOS's active-parameter threshold and not yet vetted.

**Qwen3.8 at 4, 8 and 16 bits (Ollama)**
- **Precision trades generation speed for memory and fidelity.**
  - With `xhigh` reasoning, generation runs at 39.7, 23.4 and 17.1 tokens/s at 32K for 4, 8 and 16 bits.
  - Prompt reading barely changes. 16-bit is the fastest reader (no dequantization), and 8-bit the slowest, by about 10%.
  - Fresh-session peaks at 256K are 64, 76 and 98 GiB.
- **The 4-bit build reasons much less than every higher-precision build.**
  - On the identical 32K prompt, five samples each, the median reasoning was 5,500 characters for 4-bit. The others: 10,500 (Ollama 8-bit), 15,400 (Ollama 16-bit), 15,900 (LM Studio 8-bit) and 11,800 (LM Studio 16-bit).
  - The four higher-precision setups don't differ significantly from one another. That includes the same 16-bit weights on both servers (p = 0.55).
  - Pooled, they reason 2.6× longer than 4-bit (p = 0.04).
  - The 16-bit builds are the unquantized model, so the 4-bit build is the one that departs from it.
- **Per turn, precision costs more than generation speed suggests.** An `xhigh` turn on a cached 32K prompt took 66–81 s on 4-bit, 114–242 s on 8-bit and 134–307 s on 16-bit. That is the slower generation multiplied by the longer reasoning.
- **The 16-bit build fits at 256K only from a fresh session.** It peaked at 98.1 GiB, 9 GiB under the Mac's 107.5 GiB GPU limit. With earlier conversations cached it would go over.

**LM Studio (Qwen3.8 8-bit and 16-bit)**
- **With the same 16-bit weights on both servers, Ollama generates 1.7–2× faster, and LM Studio reads prompts 2–7% faster.**
  - The generation gap is speculative decoding: Ollama uses Qwen3.8's MTP draft head, and LM Studio runs vision models without it.
  - At 8-bit the gaps are 1.4–1.6× and 6–15%.
- **At long context, LM Studio reasoned far longer than Ollama.**
  - At 128K, both LM Studio builds used about 2,700–2,800 reasoning tokens, against about 210–250 for all three Ollama builds: about 11×.
  - At 256K, LM Studio's 16-bit build used 3,916, against about 650 for Ollama's: 6×.
  - With identical 16-bit weights on both servers, that difference is the server.
  - It is one sample per build and size, but consistent across all five setups. At 32K the servers did not differ.
- **At 256K, LM Studio's 8-bit build never answered; its 16-bit build did.**
  - The 8-bit build spent the whole 16,384-token output cap enumerating modules. That took 63 minutes, and it ran about 1,800 tokens past the window without an error.
  - The 16-bit build answered in 49.8 minutes and named all 42 modules. Ollama's 16-bit build took 42.4 minutes and named 41.
  - Every Ollama build answered after 500–650 reasoning tokens.
- **LM Studio's 16-bit build fits at 256K from a fresh load, with almost no margin.** Its engine peaked at 106 GiB, 1.5 GiB under the GPU limit, against 98.1 GiB for Ollama's identical weights. Swap didn't grow, and the follow-up stayed under that peak.

## Results at a glance

**Nemotron vs Qwen3.8 (4-bit), Ollama**

| Context | Model | Prompt tokens | Time to read the prompt | Generation speed, thinking off / on | Thinking tokens (max) | Complete answer (max) | Cached follow-up (max) | Peak memory | Modules named (max) |
|---|---|---|---|---|---|---|---|---|---|
| 32K | Nemotron | 32,190 | **75 s** | **43.4 / 47.5 tok/s** | ~2,700 | **2.7 min** | **8 s** | 79.8 GiB | 1/1 |
| 32K | Qwen3.8 | 33,493 | 149 s | 40.8 / 39.7 tok/s | ~1,130 | 3.8 min | 19 s | **26.0 GiB** | 1/1 |
| 128K | Nemotron | 117,713 | **5.5 min** | **31.0 / 39.5 tok/s** | ~2,040 | **7.0 min** | — | 82.3 GiB | 14/14 |
| 128K | Qwen3.8 | 120,547 | 12.4 min | 24.5 / 26.5 tok/s | ~250 | 14.4 min | — | **41.1 GiB** | 14/14 |
| 256K | Nemotron | 242,178 | **14.3 min** | **22.0 / 30.6 tok/s** | ~1,430 | **16.6 min** | **46 s** | 85.6 GiB | 41/42 |
| 256K | Qwen3.8 | 247,553 | 36.7 min | 13.1 / 16.6 tok/s | ~550 | 40.8 min | 103 s | **64.0 GiB** | 41/42 |

**Qwen3.8 by build** (`xhigh` reasoning; values for 32K / 128K / 256K)

| Build | Time to read the prompt | Generation speed | Complete answer | Peak memory | Median reasoning at 32K (5 samples) |
|---|---|---|---|---|---|
| Ollama 4-bit NVFP4 | 149 s / 12.4 / 37.1 min | **39.7 / 26.5 / 16.6 tok/s** | **3.8 / 14.4 / 40.8 min** | **26 / 41 / 64 GiB** | 5,518 chars |
| Ollama 8-bit MXFP8 | 153 s / 12.8 / 38.5 min | 23.4 / 18.9 / 12.9 tok/s | 4.0 / 16.4 / 44.1 min | 39 / 54 / 76 GiB | 10,496 chars |
| Ollama 16-bit BF16 | 136 s / 11.7 / 35.8 min | 17.1 / 14.9 / 11.2 tok/s | 5.6 / 15.6 / 42.4 min | 61 / 76 / 98 GiB | 15,444 chars |
| LM Studio 8-bit | 145 s / 11.7 / **33.8 min** | 14.8 / 12.1 / 9.4 tok/s | 8.0 / 18.2 min / no answer | ≈42 / 59 / 88 GiB | 15,942 chars |
| LM Studio 16-bit | **134 s / 11.0 min** / 34.9 min | 8.7 / 7.7 / 6.5 tok/s | 10.8 / 22.2 / 49.8 min | 64 / 83 / 106 GiB | 11,814 chars |

- **"Max"** means Nemotron with thinking on and Qwen3.8 at `xhigh`.
- **Prompt tokens** come from the thinking-off pass. The thinking-on pass added 1–6 prompt tokens for Nemotron and about 40 for Qwen3.8 (its `xhigh` instruction). Read times matched within about 1% between passes.
- **Complete answer** runs from sending the request to the last token.
- **Ollama's peak memory** is for a fresh session: weights plus one conversation. LM Studio's is the engine process's footprint.
- **Single samples.** At 128K and 256K, each complete-answer figure is one sample. At 32K, reasoning length varied up to 5× between samples of one prompt (see the [repeat test](#reasoning-length-the-repeat-test)).

## Test setup

| | |
|---|---|
| Machine | Mac Studio (Mac16,9). Apple M4 Max with a 16-core CPU (12P + 4E) and a 40-core GPU. 128 GB unified memory, 546 GB/s. |
| GPU memory limit | Metal `recommendedMaxWorkingSetSize` is 107.5 GiB (`iogpu.wired_limit_mb` at its default) |
| OS | macOS 27.0.1 |
| Servers | Ollama 0.40.6, MLX runner (MLX 0.32.3). LM Studio 0.4.12, MLX runtime 1.13.1 (MLX 0.32.3, mlx-vlm 0.7.3). |
| Other workloads | Only one model was loaded at a time. wardsondb was stopped. OrbStack was running (about 8.5 GB). |

**Models**

| | Nemotron 3.5 Super | Qwen3.8 27B |
|---|---|---|
| Release | 2026-10-15 (NVIDIA, the VL release) | 2026-08-14 (Alibaba) |
| Design | 120B mixture of experts, 12B active per token | 27.8B dense: every parameter is used for every token |
| Layers | 88: 40 Mamba-2, 40 MoE (512 experts, 22 active), 8 attention (2 KV heads × 128) | 64: 48 Gated DeltaNet (linear attention), 16 full attention (4 KV heads × 256) |
| Maximum context | 1,048,576 tokens | 262,144 tokens |
| KV cache per token of context | 9 KiB (8 KiB + 1 KiB for the MTP head) | 68 KiB (64 KiB + 4 KiB for the MTP head) |
| Thinking levels in Ollama | off, `medium`, on (default) | off, `low`, `medium` (default), `xhigh` |

**Builds**

| Build | Tag or repo | Format | Download | Resident after load | Cold load | MTP draft head used |
|---|---|---|---|---|---|---|
| Nemotron, Ollama | `nemotron-3.5-super:120b-a12b-nvfp4` | NVIDIA's NVFP4 checkpoint (experts NVFP4, dense layers MXFP8) | 79 GB | 74.5 GiB | 13.5–17.1 s | yes |
| Qwen3.8 4-bit, Ollama¹ | `qwen3.8:27b-mlx` (= `27b-nvfp4`) | NVFP4 | 18.2 GB | 17.5 GiB | 3.3–3.6 s | yes |
| Qwen3.8 8-bit, Ollama | `qwen3.8:27b-mxfp8` | MXFP8 (8-bit float, shared scales) | 31.7 GB | 30.1 GiB | 2.3–5.6 s | yes |
| Qwen3.8 16-bit, Ollama | `qwen3.8:27b-mlx-bf16` | BF16, unquantized | 55.6 GB | 52.3 GiB | 8.0–9.6 s | yes |
| Qwen3.8 8-bit, LM Studio | `qwen/qwen3.8-27b` (lmstudio-community MLX 8-bit) | MLX affine 8-bit | 29.5 GB | 27.5 GiB | 9.0 s | no (not on LM Studio's vision-model path) |
| Qwen3.8 16-bit, LM Studio | `qwen3.8-27b` (`mlx-community/Qwen3.8-27B-bf16`) | BF16, unquantized | 54.7 GB | 52 GiB | 15 s | no |

¹ The 4-bit build was removed from this Mac after testing, replaced by the 8-bit and 16-bit builds.

**Workload**

- **Documents.** The text is the Python 3.9 standard library source, concatenated into an 11.0M-character corpus. The three documents are slices of it:

  | Document | Characters | Corpus offset | Contents |
  |---|---|---|---|
  | 32K | 121,061 | 200,000 | part of one file, `_pydecimal.py` |
  | 128K | 484,247 | 1,000,000 | 14 modules |
  | 256K | 968,494 | 3,000,000 | 42 modules |

  The sizes were calibrated on Nemotron's tokenizer. Every model and build received identical text; Qwen's tokenizer produced 2–4% more tokens from it.
- **Prompt.** Each document is followed by a question: *"Write a detailed, multi-paragraph overview of the modules it contains and what each one is for."* Each prompt starts with a random line, so no request can reuse another's cache.
- **Follow-up.** After the 32K and 256K documents, a follow-up is sent with the whole conversation, to measure prefix-cache reuse: *"In one short paragraph: which of those modules would you use to parse command-line arguments, and why?"*
- **Sequence for each build and pass.** Warmup, 32K, 32K follow-up, 128K, 256K, 256K follow-up. Both 16-bit builds started their 256K request from a fresh load, with nothing else cached. Ollama's build was reloaded just before the request. LM Studio's ran 256K and its follow-up separately, after a fresh load and the warmup.
- **Ollama requests.** Native `/api/chat` with streaming, using each model's default sampling. Qwen3.8 ships temperature 1.0, top_k 20 and top_p 0.95. Nemotron has no presets, so it gets Ollama's defaults of 0.8, 40 and 0.9.
  - **Thinking off:** `think: false`, with 200 output tokens (64 for follow-ups).
  - **Thinking on:** Nemotron `think: true`, Qwen3.8 `think: "xhigh"`. Output was capped at 16,384 tokens (8,192 for follow-ups), thinking included.
- **LM Studio requests.** Native `/api/v1/chat` with streaming and `reasoning: "off"` or `"xhigh"`. Sampling was pinned to Qwen3.8's values above (with min_p 0 and repeat penalty 1), and follow-ups were chained with `previous_response_id`. The model was loaded at 262,144 context with LM Studio's default of 4 parallel slots and KV cache quantization off.
- **Why those levels match embraOS.** embraOS sends `reasoning_effort` through the OpenAI-compatible endpoint.
  - On Ollama, `xhigh` reaches Qwen3.8 as a system instruction: "Reasoning effort is set to xhigh. Please think carefully…". LM Studio's template adds the same instruction.
  - Nemotron has no level above on, so `max` or `xhigh` falls back to its default, which is on (`ThinkingFromReasoningEffort` and `ResolveThinking` in Ollama's source).
- **Measurements.**
  - **Ollama:**
    - Prompt and generation durations are Ollama's own (`prompt_eval_duration` and `eval_duration`).
    - Peak and held memory come from the MLX runner's log line for each request.
    - Speculative-decoding acceptance comes from the runner's stats line.
  - **LM Studio:**
    - Token counts, exact reasoning-token counts and time to first token come from its own stats; time to first token stands in for prompt reading.
    - Memory is the engine process's physical footprint (2-second samples plus macOS's lifetime peak).
  - **Both:**
    - Time to the answer comes from the stream timestamps.
    - System memory, swap and pressure were sampled every 2–5 seconds. A watchdog would have unloaded the model on sustained critical pressure or more than 12 GiB of swap growth. It never fired.

## Results: Nemotron vs Qwen3.8 (4-bit), thinking off

| Context | Model | Prompt tokens | Time to read the prompt | Read speed | Generation speed (200 tokens) | Cached follow-up | Peak memory¹ |
|---|---|---|---|---|---|---|---|
| 32K | Nemotron | 32,190 | **75 s** | **427 tok/s** | **43.4 tok/s** | 0.8 s | 79.8 GiB |
| 32K | Qwen3.8 | 33,493 | 149 s | 225 tok/s | 40.8 tok/s | **0.4 s** | **26.0 GiB** |
| 128K | Nemotron | 117,713 | **5.5 min** | **354 tok/s** | **31.0 tok/s** | — | 82.3 GiB |
| 128K | Qwen3.8 | 120,547 | 12.4 min | 162 tok/s | 24.5 tok/s | — | **41.1 GiB** |
| 256K | Nemotron | 242,178 | **14.3 min** | **282 tok/s** | **22.0 tok/s** | **1.6 s** | 85.6 GiB |
| 256K | Qwen3.8 | 247,553 | 36.7 min | 112 tok/s | 13.1 tok/s | 3.5 s | **64.0 GiB** |

¹ The MLX runner's peak for a fresh session. Earlier benchmark conversations that the prompt cache kept in memory are subtracted, so this is weights plus one conversation. The raw peaks were Nemotron 79.8 / 84.0 / 91.1 GiB and Qwen3.8 26.0 / 44.5 / 77.7 GiB.

## Results: Nemotron vs Qwen3.8 (4-bit), thinking at the maximum level

| Context | Model | Time to read the prompt | Thinking tokens | Answer tokens | Generation speed | Answer starts | Answer complete | Peak memory | Draft acceptance |
|---|---|---|---|---|---|---|---|---|---|
| 32K | Nemotron | **75 s** | ~2,700 | ~1,320 | **47.5 tok/s** | **2.2 min** | **2.7 min** | 79.8 GiB | 0.91 |
| 32K | Qwen3.8 `xhigh` | 149 s | ~1,130 | ~2,080 | 39.7 tok/s | 3.0 min | 3.8 min | **26.0 GiB** | 0.67 |
| 128K | Nemotron | **5.5 min** | ~2,040 | ~1,490 | **39.5 tok/s** | **6.4 min** | **7.0 min** | 82.3 GiB | 0.92 |
| 128K | Qwen3.8 `xhigh` | 12.4 min | ~250 | ~2,860 | 26.5 tok/s | 12.6 min | 14.4 min | **41.2 GiB** | 0.74 |
| 256K | Nemotron | **14.3 min** | ~1,430 | ~2,900 | **30.6 tok/s** | **15.1 min** | **16.6 min** | 85.6 GiB | 0.91 |
| 256K | Qwen3.8 `xhigh` | 37.1 min | ~550 | ~3,080 | 16.6 tok/s | 37.8 min | 40.8 min | **63.9 GiB** | 0.77 |

**Follow-ups with thinking on.** Only the tokens added since the cached prompt are read: the previous answer plus the new question.

| Follow-up | Model | New tokens read | Read time | Thinking + answer tokens | Complete |
|---|---|---|---|---|---|
| 32K | Nemotron | 1,360 | 3.5 s | ~130 + ~80 | **8 s** |
| 32K | Qwen3.8 `xhigh` | 2,109 | 10.8 s | ~230 + ~120 | 19 s |
| 256K | Nemotron | 2,939 | 14.8 s | ~920 + ~70 | **46 s** |
| 256K | Qwen3.8 `xhigh` | ~3,080 | 45.7 s | ~740 + ~280 | 103 s |

Ollama's thinking and answer token counts are estimates. They split the generated-token count by the share of streamed chunks in each phase.

**Module coverage.** For each first answer, these are the standard-library modules in the document that the answer names (in backticks, bold or a heading):

| Document | Modules in it | Nemotron | Qwen3.8 `xhigh` |
|---|---|---|---|
| 32K | 1 (`_pydecimal`, the pure-Python `decimal`) | 1, identified as `decimal` | 1, identified as `decimal` |
| 128K | 14 | 14 | 14 |
| 256K | 42 (17 in the first third, 14 in the middle, 11 in the last) | 41: missed one in the middle third | 41: missed one in the first third |

Neither model named a module that was not in the document. Every other module name was a cross-reference, such as `smtpd` "built on `asyncore`/`asynchat`" or `shelve` "on top of `dbm`".

## Qwen3.8 at 4, 8 and 16 bits on Ollama

The same benchmark was run on Ollama's 8-bit (`qwen3.8:27b-mxfp8`) and 16-bit (`qwen3.8:27b-mlx-bf16`) builds, from 19:24 on 2026-10-21 to 00:01 the next morning. All three builds include the MTP draft head.

**Thinking off**

| Context | Measure | 4-bit | 8-bit | 16-bit |
|---|---|---|---|---|
| 32K | Time to read the prompt | 149 s | 153 s | **136 s** |
| 32K | Generation speed | **40.8 tok/s** | 24.4 tok/s | 18.9 tok/s |
| 32K | Peak memory (fresh session) | **26.0 GiB** | 38.5 GiB | 60.8 GiB |
| 128K | Time to read the prompt | 12.4 min | 13.0 min | **11.8 min** |
| 128K | Generation speed | **24.5 tok/s** | 17.5 tok/s | 13.2 tok/s |
| 128K | Peak memory (fresh session) | **41.1 GiB** | 53.6 GiB | 76.0 GiB |
| 256K | Time to read the prompt | 36.7 min | 38.9 min¹ | **35.8 min** |
| 256K | Generation speed | **13.1 tok/s** | 10.9 tok/s | 10.1 tok/s |
| 256K | Peak memory (fresh session) | **64.0 GiB** | 76.4 GiB | 98.1 GiB² |
| — | Cached follow-up, 32K / 256K | 0.4 / 3.5 s | 0.4 / 3.5 s | 0.4 / 1.0 s |

¹ This run overlapped a 55 GB download in LM Studio. macOS briefly reached critical memory pressure and added 8 GiB of swap. The `xhigh` pass's read of the same document took 38.5 minutes, so the effect on timing was small.

² The 16-bit build was reloaded before its 256K request, so this peak covers one conversation and nothing else cached. With the 32K and 128K conversations still cached, as the other builds had, it would have exceeded the 107.5 GiB GPU limit.

**Thinking at `xhigh`**

| Context | Measure | 4-bit | 8-bit | 16-bit |
|---|---|---|---|---|
| 32K | Reasoning + answer tokens | ~1,130 + ~2,080 | ~1,030 + ~1,080 | ~2,100 + ~1,340 |
| 32K | Generation speed | **39.7 tok/s** | 23.4 tok/s | 17.1 tok/s |
| 32K | Answer complete | **3.8 min** | 4.0 min | 5.6 min |
| 128K | Reasoning + answer tokens | ~250 + ~2,860 | ~210 + ~3,910 | ~210 + ~3,220 |
| 128K | Generation speed | **26.5 tok/s** | 18.9 tok/s | 14.9 tok/s |
| 128K | Answer complete | **14.4 min** | 16.4 min | 15.6 min |
| 256K | Reasoning + answer tokens | ~550 + ~3,080 | ~500 + ~3,810 | ~650 + ~3,710 |
| 256K | Generation speed | **16.6 tok/s** | 12.9 tok/s | 11.2 tok/s |
| 256K | Answer complete | **40.8 min** | 44.1 min | 42.4 min |
| 256K | Follow-up complete | **103 s** | 135 s | 179 s |
| — | Draft acceptance | 0.67–0.88 | 0.53–0.81 | 0.47–0.76 |

**Read speed as the context grows**, from a fit over about 178 chunks of 2,048 tokens:

| Build | Seconds per prompt token | at 2K | at 32K | at 128K | at 240K |
|---|---|---|---|---|---|
| 4-bit | 3.60 ms + 42.7 ns × position | 272 tok/s | 200 tok/s | 109 tok/s | 72 tok/s |
| 8-bit | 3.75 ms + 45.4 ns × position | 260 tok/s | 191 tok/s | 103 tok/s | 68 tok/s |
| 16-bit | 3.18 ms + 44.2 ns × position | **306 tok/s** | **216 tok/s** | **111 tok/s** | **72 tok/s** |

- **Reading.** The 16-bit build reads fastest at short contexts because its weights skip dequantization. By 240K the attention cost, which is the same for every build, dominates and the three converge.
- **Generating.** Generation is limited by how much weight data each token reads, so 4-bit leads: 1.3–1.7× faster than 8-bit and 1.5–2.3× faster than 16-bit with `xhigh`. The gap narrows at long contexts, where reading the KV cache (about 16 GiB per token at 250K, the same for every build) takes over.
- **Memory.** The fresh-session peaks are the resident weights (17.5, 30.1 and 52.3 GiB) plus the same context-related memory: about 8.5, 23.5 and 46.5 GiB at 32K, 128K and 256K.
- **Memory pressure.**
  - 8-bit: the critical moment above, then a 15-second "warning" during its `xhigh` 256K read with 3 GiB of swap added.
  - 16-bit: pressure stayed normal in both passes, peaking at 98.1 GiB (98.9 GiB in its `xhigh` 256K follow-up).
- **Short reasoning at long context.** Every build reasoned only 200–650 tokens on the 128K and 256K documents, against 1,000–2,100 at 32K (one sample each).

## LM Studio: Qwen3.8 27B (8-bit)

The same documents, sequence and output caps were run on LM Studio from 15:09 to 17:31 on 2026-10-21. LM Studio is the server embraOS vets Qwen3.8 on. The closest Ollama comparison is the 8-bit MXFP8 build. Both are 8-bit, though in different formats: MLX affine integers for LM Studio, 8-bit floats for Ollama.

**Thinking off**

| Context | Time to read the prompt (LM Studio / Ollama 8-bit) | Generation (LM Studio / Ollama 8-bit) | Cached follow-up (LM Studio / Ollama 8-bit) | Peak memory (LM Studio / Ollama 8-bit, raw) |
|---|---|---|---|---|
| 32K | **145 s** / 153 s | 15.1 / **24.4 tok/s** | 0.8 / **0.4 s** | ≈37 / 38.5 GiB |
| 128K | **11.7** / 13.0 min | 12.3 / **17.5 tok/s** | — | ≈56–60 / 56.9 GiB |
| 256K | **33.8** / 38.9 min | 9.6 / **10.9 tok/s** | **2.1** / 3.5 s | 88 / 89.9 GiB |

**Thinking at `xhigh`**

| Context | Time to read the prompt (LM Studio / Ollama 8-bit) | Reasoning tokens (LM Studio / Ollama 8-bit) | Generation (LM Studio / Ollama 8-bit) | Complete answer (LM Studio / Ollama 8-bit) | Follow-up complete (LM Studio / Ollama 8-bit) | Peak memory, LM Studio | Modules named, LM Studio |
|---|---|---|---|---|---|---|---|
| 32K | **145 s** / 153 s | 3,579 / ~1,030 | 14.8 / **23.4 tok/s** | 8.0 / **4.0 min** | 47 / **29 s** | 42 GiB | 1/1 |
| 128K | **11.7** / 12.8 min | 2,675 / ~210 | 12.1 / **18.9 tok/s** | 18.2 / **16.4 min** | — | 59 GiB | 14/14 |
| 256K | **33.8** / 38.5 min | 16,383 (the cap) / ~500 | 9.4 / **12.9 tok/s** | **no answer** after 62.7 min / 44.1 min | **99** / 135 s | 88 GiB; 104 GiB in the follow-up | 0/42 (no answer) |

- **Speed.** LM Studio reads prompts 6–15% faster than Ollama's 8-bit build. Ollama generates 1.4–1.6× faster with `xhigh`, because it uses its MTP draft head and LM Studio's vision-model path runs no speculative decoding.
- **Memory.** It is about the same once weights are counted: LM Studio's 8-bit weights are 2.6 GiB smaller, and its peaks match Ollama's raw peaks within about 2 GiB.
- **Measurement differences.** LM Studio's reasoning counts are exact and Ollama's are estimates. LM Studio's memory is the engine process's footprint. Its first two thinking-off figures are estimated from wired memory, because the footprint sampler started later.

**16-bit build** (`mlx-community/Qwen3.8-27B-bf16`, 00:24–01:28 on 2026-10-22; 256K at 07:02–07:56)

- **Only the `xhigh` pass ran.**
  - This repo has no LM Studio model definition, so LM Studio rejects the API's `reasoning` setting: "Model 'qwen3.8-27b' does not expose reasoning configuration".
  - Without that setting, Qwen3.8's template defaults to `xhigh` with thinking on. The requests matched the 8-bit build's `xhigh` prompts token for token, but thinking could not be turned off.
- **256K ran separately, from a fresh load,** as on Ollama's 16-bit build: a 4,516-token warmup, then 256K and its follow-up.
  - It was first skipped on an estimate of about 110 GiB.
  - That estimate started from the 8-bit build's 256K peak, which included memory still held from its 128K turn.

| Context | Measure | LM Studio 16-bit | Ollama 16-bit (same weights) |
|---|---|---|---|
| 32K | Time to read the prompt | **134 s** | 136 s |
| 32K | Reasoning + answer tokens | 3,141 + 1,331 | ~2,100 + ~1,340 |
| 32K | Generation speed | 8.7 tok/s | **17.1 tok/s** |
| 32K | Answer complete | 10.8 min | **5.6 min** |
| 32K | Follow-up complete | 42 s | **22 s** |
| 32K | Peak memory | 64 GiB | **61 GiB** |
| 128K | Time to read the prompt | **11.0 min** | 11.7 min |
| 128K | Reasoning + answer tokens | 2,829 + 2,308 | ~210 + ~3,220 |
| 128K | Generation speed | 7.7 tok/s | **14.9 tok/s** |
| 128K | Answer complete | 22.2 min | **15.6 min** |
| 128K | Peak memory | 83 GiB | **80 GiB** (raw) |
| 256K | Time to read the prompt | **34.9 min** | 35.8 min |
| 256K | Reasoning + answer tokens | 3,916 + 1,883 | ~650 + ~3,700 |
| 256K | Generation speed | 6.5 tok/s | **11.2 tok/s** |
| 256K | Answer complete | 49.8 min | **42.4 min** |
| 256K | Follow-up complete | 186 s | **179 s** |
| 256K | Peak memory (fresh load) | 106 GiB | **98 GiB** |
| 256K | Modules named | **42/42** | 41/42 |

- **Same weights, different servers.** Ollama generates 1.7–2× faster, which is the effect of its MTP draft head. LM Studio reads prompts 2–7% faster. Both named every module at 32K and 128K.
- **Memory is close up to 128K and tight at 256K.**
  - LM Studio's engine footprint was 3 GiB above Ollama's raw MLX peak at 32K and 128K.
  - At 256K, both from a fresh load, LM Studio peaked at 106 GiB against Ollama's 98.1 GiB.
  - LM Studio's peak rose about 0.25 GiB per 1,000 tokens read until 88% of the read, then held at 105–106 GiB to the end. That suggests the engine releasing cached scratch memory near the limit.
  - System wired memory, sampled the same way on both servers, peaked at 107.3 GiB against 92 GiB in Ollama's 256K read.
  - Swap didn't grow and memory pressure stayed normal. The follow-up peaked at 104 GiB, under the first turn.
- **Long-context reasoning.** With the weights held equal, the difference belongs to the server:
  - At 128K, LM Studio's 16-bit build reasoned for 2,829 tokens, like its 8-bit build's 2,675, while Ollama's identical weights reasoned for about 210.
  - At 256K it reasoned for 3,916 tokens against about 650.
- **Follow-ups replay the latest reasoning even without LM Studio's definition.**
  - The 32K follow-up read 4,757 new tokens: the previous reasoning, the answer and the question.
  - The 256K follow-up read 5,873 new tokens in 94 s. Its replay fit, unlike the 8-bit build's.

**What happened at 256K with `xhigh` (8-bit build):**
- **A reasoning loop.** After planning its answer, Qwen3.8 started writing one line per module through the alphabet ("Need maybe mention "shlex": shlex."). It was still in the S's when the 16,384-token cap stopped it, 63 minutes in, with no answer written.
  - All three Ollama builds answered the same prompt after 500–650 reasoning tokens.
  - LM Studio's 16-bit build, run later, answered it after 3,916.
  - With one sample each, this doesn't show that the 8-bit build is more prone to looping.
- **Past the window, silently.**
  - The 247,589-token prompt plus 16,383 output tokens comes to 263,972, about 1,800 tokens past the 262,144-token context the model was loaded with. LM Studio raised no error and logged nothing.
  - These requests set no overflow policy. embraOS's LM Studio preset sets `contextOverflowPolicy: stopAtLimit`, which would have ended the turn at the window with an error.
- **The follow-up changed shape.** Chaining replays the previous turn.
  - At 32K that replay included the reasoning: 5,249 new tokens to read, against 2,109 in the Ollama 4-bit run, which sent back only the answer.
  - At 256K the replay would not have fit, so LM Studio silently dropped the 16,383 reasoning tokens. The follow-up was exactly the size of the original prompt, and the model answered in 750 tokens without reasoning, despite `xhigh`.
- **Memory.** That follow-up took the engine to 104 GiB, close to the 107.5 GiB GPU limit. macOS memory pressure stayed normal.

## Reasoning length: the repeat test

Reasoning length varies a lot between samples, so the 32K `xhigh` request was repeated four more times for each Qwen3.8 setup. Setup:
- Every repeat used the byte-identical prompt with a fixed first line.
- Sampling was identical everywhere, and output was capped at 8,192 tokens.
- After the first request the prompt came from cache, so repeats measure generation only.
- All setups counted exactly the same 33,516 prompt tokens.

Each group below adds that setup's original 32K `xhigh` run as a fifth sample.

| Setup | Reasoning, characters (5 samples) | Median reasoning | Median answer | `xhigh` turn, prompt cached |
|---|---|---|---|---|
| Ollama 4-bit | 2,415 · 5,224 · 5,518 · 10,605 · 11,955 | **5,518** | 10,489 chars | **66–81 s** |
| Ollama 8-bit | 4,998 · 6,603 · 10,496 · 17,801 · 19,321 | 10,496 | 7,442 chars | 114–242 s |
| Ollama 16-bit | 5,846 · 10,072 · 15,444 · 16,214 · 21,119 | 15,444 | 6,198 chars | 134–307 s |
| LM Studio 8-bit | 10,201 · 13,470 · 15,942 · 16,885 · 18,445 | 15,942 | 6,371 chars | 272–390 s |
| LM Studio 16-bit | 4,790 · 11,654 · 11,814 · 14,712 · 15,616 | 11,814 | 6,394 chars | 220–445 s |

Exact two-sided Mann-Whitney tests, five samples per side:

| Comparison | p |
|---|---|
| Ollama 4-bit vs LM Studio 8-bit | 0.03 |
| Ollama 4-bit vs Ollama 16-bit | 0.10 |
| Ollama 4-bit vs Ollama 8-bit | 0.42 |
| Ollama 8-bit vs Ollama 16-bit | 0.84 |
| Ollama 8-bit vs LM Studio 8-bit | 0.69 |
| Ollama 16-bit vs LM Studio 8-bit | 0.69 |
| Ollama 16-bit vs LM Studio 16-bit (identical weights) | 0.55 |
| LM Studio 8-bit vs LM Studio 16-bit | 0.22 |
| Ollama 4-bit vs all four higher-precision setups pooled (20 samples, median 14,091) | **0.04** |

- **Precision, not the server, drives reasoning length at 32K.**
  - The four higher-precision setups are indistinguishable from one another, on either server, including identical 16-bit weights on both (p = 0.55).
  - Pooled, they reason 2.6× longer than the 4-bit build, and the 4-bit build writes longer answers.
  - The 16-bit builds are the unquantized model, so they are the reference; the 4-bit build is the one that departs from them.
  - Whether thinking less makes the 4-bit build's answers worse was not measured.
- **An earlier theory doesn't hold.** It was that Ollama's sampler, which applies top-k before top-p, accounted for the difference. Ollama's own 8-bit and 16-bit builds reasoned as long as LM Studio's.
- **MTP isn't the cause either.** Ollama's draft head samples from the same filtered distribution its acceptance test uses (`mlxrunner/mtp.go`, `speculate.go`). That is exact speculative sampling, so it leaves the output distribution unchanged.
- **At long context the server matters.**
  - At 128K, both LM Studio builds reasoned for about 2,700–2,800 tokens, against about 210–250 for all three Ollama builds. That holds even with identical 16-bit weights on both servers.
  - At 256K, LM Studio's 8-bit build looped where every Ollama build answered after 500–650 reasoning tokens.
  - Each of these is one sample per build. They are consistent across all five setups, but not proven.

## Analysis

### Reading the prompt

- **Reading cost grows in a straight line with position.** The fits are in the tables above. Nemotron is 2.14 ms + 11.6 ns × position per token; Qwen3.8 is 3.2–3.8 ms + 43–45 ns × position.
- **Why Qwen3.8 is slower to read.** It's dense, so it reads with all 27.8B parameters against Nemotron's 12B. Its 16 full-attention layers also cost about 3.7× more per position than Nemotron's 8: 24 query heads of width 256 against 32 of width 128.
- **The results repeat.** Read times in the thinking-off and thinking-on passes matched within about 1% for every build, including the 8-bit 256K run that overlapped a download.
- **Nemotron beyond 256K, extrapolated.** About 45 minutes for 512K and 2.4 hours for 1M on a cold read. Peak memory is about 79 GiB + 29 KiB per token, which reaches the GPU limit near 1M, so the practical ceiling on this Mac is about 850K tokens with a margin.

### Generating

- **Steady-state speeds.** The thinking-on runs generated 1,000–4,300 tokens each, which gives a truer steady-state speed than the 200-token thinking-off runs. Nemotron's steady-state speed was up to 39% higher than its 200-token figure (30.6 vs 22.0 tok/s at 256K).
- **What sets the speed.** Each new token reads the weights and the whole KV cache.
  - For Qwen3.8 the weights are about 15, 28 or 50 GiB depending on precision, and the cache about 16 GiB at 250K.
  - For Nemotron the active weights are about 7 GiB and its cache about 2 GiB at 242K.
- **MTP draft acceptance** was highest on Nemotron (0.90–0.93 with thinking on). On Qwen3.8 it ran 0.47–0.88 across builds, partly because Qwen3.8 samples at temperature 1.0 against Nemotron's 0.8. LM Studio doesn't use the draft head at all. With identical 16-bit weights, that alone made Ollama 1.7–2× faster at generating.

### Thinking at max effort

- **Thinking was short on this task.** Nemotron thought for 1.4–2.7K tokens per answer; Qwen3.8 for 0.2–4.5K, depending on build and sample. Thinking was shortest on the long documents.
- **Hard tasks think much longer.** Across its evaluation suite, Artificial Analysis measured about 49K output tokens per task for Nemotron and about 67K for Qwen3.8 at `xhigh`. At the speeds measured here:

  | Output tokens | Nemotron at 32K / 128K / 256K | Qwen3.8 4-bit | Qwen3.8 8-bit | Qwen3.8 16-bit |
  |---|---|---|---|---|
  | 10,000 | 3.5 / 4.2 / 5.5 min | 4.2 / 6.3 / 10.0 min | 7.1 / 8.8 / 12.9 min | 9.7 / 11.2 / 14.9 min |
  | 50,000 | 17.5 / 21.1 / 27.3 min | 21.0 / 31.5 min / does not fit | 35.6 / 44.2 min / does not fit | 48.6 / 55.9 min / does not fit |

  In an agentic turn with max reasoning, generation, not prompt reading, is likely to dominate at small and medium contexts.

### Context headroom with max reasoning

- **Qwen3.8's window holds the whole turn.** Its 262,144 tokens must hold the prompt, the thinking and the answer.
- **How each server enforces it.**
  - Ollama's MLX runner refuses a prompt at or above the window; embraOS sees this as HTTP 400 (`mlxrunner/pipeline.go`). It also stops generating when the window fills.
  - LM Studio, without a stop-at-limit policy, kept generating past it in the 8-bit 256K run.
- **At 256K there was little room left.** The 256K document took about 247,600 tokens, leaving about 14,500 for thinking and answer. The Ollama builds used 3,600–4,400 of them. A hard agentic turn would not fit.
- **embraOS's fixed overhead.** Each embraOS request carries about 20,000 tokens of tool manifest (RECOMMENDED-LOCAL-MODELS.md). The history and tool results then share the remaining ~240K with each turn's reasoning.
- **What to plan around.** To keep max-reasoning turns from hitting the window, a Qwen3.8 session needs tens of thousands of tokens free. In practice, start a new session well before 200K. A warning from the brain at a threshold like that, as it already does for short GGUF windows, would make this visible.
- **Nemotron has headroom.** Its 1M window (about 850K usable here) leaves plenty, at the cost of long cold reads.

### Multi-turn sessions and the prompt cache

- **Each turn reads only its new tokens.** embraOS replays the whole session every turn, and with the model loaded, the prefix cache means only the new tokens are read.
  - At 250K of context, reading 3,000–3,800 new tokens took 15 s on Nemotron and 46–57 s on Qwen3.8.
  - A cold start, after a reload or a cache eviction, re-reads everything: 14 min for Nemotron and 34–39 min for Qwen3.8 at about 250K.
- **Keep models loaded.** `OLLAMA_KEEP_ALIVE=-1` (already set on this Mac) avoids most of these cold starts.
- **Reuse across conversations.** Ollama also reused the shared opening tokens across different conversations, up to where they diverged: 47 tokens here, the `xhigh` instruction plus the start of the message. A fixed system prompt and tool manifest may therefore be reused across embraOS sessions, but that is untested.
- **The two servers cache differently.** This was checked in LM Studio's engine code and logs.

  | | Ollama (MLX runner) | LM Studio (MLX engine, vision models such as Qwen3.8) |
  |---|---|---|
  | Where the cache lives | Unified memory: Qwen3.8 held 38–73 GiB after the 256K runs, depending on build | An anonymous temp file on SSD under `/tmp`, loaded back on a hit |
  | Save points | The end of each user message | Every 256 tokens |
  | Reuse across conversations | Seen for the shared opening tokens | Any shared beginning |
  | Limit | Available memory | A disk budget: 162.8 GiB for a 262K prompt in this Mac's log; at least 10 GiB of disk is left free |
  | Survives a restart | No | No |
  | Seen in real use | This benchmark | Sessions of about 98K tokens restored with 484–2,182 new tokens each (this Mac's log, 2026-10-17) |

### Memory

- **Resident weights and peaks:**

  | Build | Resident weights | Peak by 256K |
  |---|---|---|
  | Nemotron | 74.5 GiB | 80–86 GiB |
  | Qwen3.8 4-bit | 17.5 GiB | 64 GiB |
  | Qwen3.8 8-bit | 30.1 GiB | 76 GiB (fresh session); 90 GiB with earlier conversations cached |
  | Qwen3.8 16-bit | 52.3 GiB | 98 GiB on Ollama and 106 GiB on LM Studio, fresh session only |
- **On this 128 GB Mac:**
  - Nemotron and the 16-bit Qwen3.8 leave no room for LM Studio's Qwen3.8 (about 50 GB at 262K × 4 parallel) or wardsondb (about 19 GB).
  - The 8-bit Qwen3.8 fits alongside them at moderate contexts.
- **Every 256K run pushed free memory close to zero.** macOS absorbed it by compressing and dropping cached pages. Critical pressure appeared only once: the 8-bit run that overlapped a download.

### Quality and reception

Artificial Analysis Intelligence Index v4.3.2, read 2026-10-21:

| | Nemotron 3.5 Super VL | Qwen3.8 27B (`xhigh`) | Qwen3.8 27B (`medium`) |
|---|---|---|---|
| Intelligence Index | 26 | **34** | 28 |
| AA-LCR (long-context reasoning) | 69% | **82%** | |
| AutomationBench-AA (agentic) | 15% | **48%** | |
| GDPval-AA | 1162 | **1417** | |
| Humanity's Last Exam | 31% | **34%** | |
| AA-Omniscience (knowledge minus hallucination) | **−4** | −10 | |
| Output tokens per task | **49K** | 67K | |

- **Nemotron's standing.** It is NVIDIA's newest release, six days old at the time of the test. Artificial Analysis calls it the top US open-weights model, and its score doubles Nemotron 3 Super's 13. Community reviews were still sparse.
- **Early tester reports.** Two DGX Spark testers on NVIDIA's forum scored it 90 and 94 out of 100 on a tool-calling benchmark. Both runs failed the same prompt-injection test.
- **Qwen3.8's standing.** Alibaba's agentic claims, such as SWE-bench Pro 61.7, are vendor-reported. It is embraOS's vetted local pick, tested against the full tool surface.
- **This benchmark doesn't separate the models on reading.** Both read and described the whole document equally well. It does not test tool use or multi-step work, which is where the scores above differ most.

### What this means for embraOS's local presets

1. **Make the 8-bit Ollama build the default local brain** (`qwen3.8:27b-mxfp8`).
   - It reasons like the unquantized model, where the 4-bit build reasoned less than half as long.
   - It fits 256K: 76 GiB in a fresh session, about 90 GiB with earlier conversations cached.
   - It generates 1.4–1.6× faster than LM Studio's 8-bit build, thanks to MTP.
   - Re-check it against the tool surface, since the vetting was done on the 4-bit Ollama and 8-bit LM Studio builds.
2. **Use the 16-bit build for maximum fidelity up to about 128K.** It reads fastest and is the model as trained. It generates slowest. At 256K it needs 98 GiB on Ollama (106 GiB on LM Studio) with nothing else cached, which long embraOS sessions would exceed.
3. **Expect longer turns at higher precision.** With `xhigh` on a cached 32K prompt, turns took 66–81 s on the 4-bit build, 2–4 minutes on 8-bit and 2–5 minutes on 16-bit. Most of that comes from the model reasoning longer, not just generating more slowly.
4. **Budget Qwen3.8 sessions for max reasoning.** The window is shared with thinking. A session near 250K has about 14K tokens left for a turn, so warn or start a new session well before that.
5. **Bound runaway reasoning.**
   - At 256K, LM Studio's 8-bit build on `xhigh` reasoned for 63 minutes without answering. Its 16-bit build answered, but after 6× Ollama's reasoning.
   - At 128K and 256K, LM Studio reasoned 6–11× longer than Ollama with the same weights. On LM Studio, expect long turns at large contexts.
   - On LM Studio, keep the `stopAtLimit` preset, since without it the window overflows silently.
   - Consider a per-turn output-token cap for local max-reasoning presets; the brain sends none today.
   - `/stop` remains the manual escape.
6. **Keep models resident.** With `OLLAMA_KEEP_ALIVE=-1`, each turn costs only its new tokens. A reload re-reads the whole session: 34–39 minutes at 250K.
7. **Decide whether earlier reasoning is replayed.**
   - Sending previous turns' reasoning back lengthens every follow-up (5,249 vs 2,109 new tokens at 32K) and fills the window faster.
   - With the OpenAI-compatible endpoint embraOS uses, that is the brain's choice.
   - LM Studio's own definition for Qwen3.8 turns "Preserve Thinking" on by default. That keeps the reasoning of every earlier turn in the template, whenever it is sent back.
8. **Nemotron 3.5 Super is the fast, long-context option, but unvetted.** It reads 2–2.6× faster, generates up to 1.8× faster with thinking on, and goes past 262K. It stays "under evaluation": 12B active parameters puts it below embraOS's active-parameter threshold, and its agentic scores are well below Qwen3.8's. It is a candidate for long-document or "fast" presets once it has been run against the tool surface.

### LM Studio notes

- **Qwen3.8 16-bit.**
  - LM Studio has no official 16-bit build; lmstudio-community publishes only 4-, 6- and 8-bit. The build used here is `mlx-community/Qwen3.8-27B-bf16`.
  - To expose reasoning levels through the API, it would need a model definition like the one for `qwen/qwen3.8-27b` in `~/.lmstudio/hub/models/`.
- **Nemotron (not runnable yet).** The matching build is `nativ-community/NVIDIA-Nemotron-3.5-Super-VL-120B-A12B-NVFP4` (80.2 GB).
  - It comes from the same NVIDIA NVFP4 checkpoint as the Ollama tag, includes the MTP head and vision encoder, and its greedy output matched NVIDIA's checkpoint token for token.
  - LM Studio's runtime 1.13.1 (mlx-vlm 0.7.3) has no `nemotron_h_omni` model package; importing it in LM Studio's own Python fails with `ModuleNotFoundError`. The build was tested by its maker with mlx-vlm 0.7.8. It was downloaded on 2026-10-21 and will run once an LM Studio runtime adds the architecture.
  - `Duckets/…-MLX-4bit` (69.4 GB) is a different quantization, standard MLX 4-bit made from the BF16 release, with no MTP head.
- **LM Studio's vision path.**
  - LM Studio loads every model with a `vision_config`, which includes Qwen3.8 and Nemotron VL, through its batched vision path. That path runs no speculative decoding.
  - It also refuses KV cache quantization: "The mlx-vlm batched vision path does not support KV cache quantization yet".

## Caveats

- **Single runs.**
  - Each configuration ran once, except the 32K `xhigh` repeat test (five samples per Qwen3.8 setup).
  - Read times repeated within about 1%. Reasoning length varied up to 5× between samples of one prompt, so single-run reasoning comparisons, including everything at 128K and 256K, are indicative only.
- **One task.** The task was a document overview, not tool calling. How long a model thinks depends heavily on the task, and agentic turns usually think far longer.
- **Estimated split on Ollama.** The thinking/answer token split is estimated from streamed chunks, because speculative decoding emits several tokens per chunk. LM Studio's split is exact.
- **Sampling.** Each model used its own default sampling, which affects speculative-decoding acceptance and so generation speed. LM Studio was pinned to Qwen3.8's Ollama defaults.
- **Two different 8-bit formats.** Ollama's MXFP8 (8-bit floats with shared scales) and LM Studio's MLX affine 8-bit integers are both near-lossless, but not identical.
- **Memory measurement.**
  - Ollama's figures are the MLX runner's own peaks. "Fresh session" subtracts earlier cached conversations, except for the 16-bit 256K runs, which really did start fresh.
  - LM Studio's figures are the engine process's footprint, sampled every 2 seconds at 1 GiB resolution plus macOS's lifetime peak. Its first two thinking-off figures are estimated from wired memory.
  - System wired memory was sampled the same way on both servers.
- **Interference.**
  - The 8-bit thinking-off 256K run overlapped a 55 GB download.
  - The start of the LM Studio thinking-off pass overlapped a Nemotron download.
  - Both used disk and network only, but macOS memory behavior differed, so the pressure observations are indicative.
- **Follow-ups were built differently on each server.** LM Studio's response chaining replays earlier reasoning when it fits, while Ollama's follow-ups sent back only the answer.
- **LM Studio's 16-bit build ran a reduced set.** It ran only the `xhigh` pass, because its repo exposes no reasoning setting and thinking could not be turned off. Its 256K request ran separately, from a fresh load.
- **Module coverage is a crude check.** It counts module names in backticks, bold or headings; it is not a quality score.
- **Third-party scores change.** The Artificial Analysis figures may change as evaluations are updated.

## Sources

- Artificial Analysis comparisons: [Nemotron 3.5 Super VL vs Qwen3.8 27B](https://artificialanalysis.ai/models/comparisons/nemotron-3-5-super-vl-vs-qwen3-8-27b), [Qwen3.8 27B (medium)](https://artificialanalysis.ai/models/qwen3-8-27b-medium), [launch article](https://artificialanalysis.ai/articles/nemotron-3-5-super-and-bolt)
- [NVIDIA Nemotron 3.5 Super VL model card](https://huggingface.co/nvidia/NVIDIA-Nemotron-3.5-Super-VL-120B-A12B-BF16) and [NVFP4 release](https://huggingface.co/nvidia/NVIDIA-Nemotron-3.5-Super-VL-120B-A12B-NVFP4)
- [NVIDIA Developer Forums: early DGX Spark tester thread](https://forums.developer.nvidia.com/t/nvidia-nemotron-3-5-super-vl-120b-a12b-released-with-spark-dedicated-vllm-config-lilicorr-speculative-decoding-model/385969)
- [embraOS: RECOMMENDED-LOCAL-MODELS.md](https://github.com/Ward-Software-Defined-Systems/embraOS/blob/main/docs/LOCAL-MODELS/RECOMMENDED-LOCAL-MODELS.md)
- Ollama library tags: [qwen3.8](https://ollama.com/library/qwen3.8/tags) and [qwen3.6](https://ollama.com/library/qwen3.6/tags)
- Ollama v0.40.6 source:
  - `openai/openai.go` (`ThinkingFromReasoningEffort`)
  - `model/renderers/thinking.go` (`ResolveThinking`)
  - `model/renderers/qwen35.go` (the `xhigh` instruction)
  - `mlxrunner/pipeline.go` (context limit, memory log)
  - `mlxrunner/mtp.go` and `mlxrunner/speculate.go` (draft sampling and acceptance)
  - `mlxrunner/sample/sample.go` (sampler order)
- LM Studio MLX engine (runtime 1.13.1): `mlx_engine/generate.py`, `mlx_engine/utils/sampling.py`, `mlx_engine/model_kit/batched_vision/prompt_cache/`
- LM Studio REST API: [`POST /api/v1/chat`](https://lmstudio.ai/docs/developer/rest/chat) and [streaming events](https://lmstudio.ai/docs/developer/rest/streaming-events)
- MLX builds: [mlx-community Qwen3.8-27B-bf16](https://huggingface.co/mlx-community/Qwen3.8-27B-bf16), [nativ-community Nemotron NVFP4](https://huggingface.co/nativ-community/NVIDIA-Nemotron-3.5-Super-VL-120B-A12B-NVFP4), [Duckets Nemotron MLX-4bit](https://huggingface.co/Duckets/NVIDIA-Nemotron-3.5-Super-VL-120B-A12B-MLX-4bit)
