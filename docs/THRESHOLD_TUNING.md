# AST and CLAP threshold tuning (clap-htsat-unfused)

CLAP cosine similarities with the Xenova/HF `clap-htsat-unfused` ONNX are lower than the scale an initial floor of 0.45
(and an SFX threshold of 0.25) assumed: with those values 2 of 4 integration-test files got no auto-tags. The thresholds
below were measured on a real library and are the current defaults.

## Data / method
- Library copy of 5309 sounds. Confirmed user `tags` are only 6 bulk tags ("hum", "field recording"...), unusable
  as ground truth; the existing `suggested_tags` cover 89 files, mostly "music" (a weak reference, not produced by this model).
- Sample: 306 files with full AST+CLAP (14 per collection, plus all files with existing suggested/confirmed tags) and
  1200 random files with CLAP embedding only (for SFX distributions). Total 1506, no decode failures.
- Models: fp32 AST, CLAP ONNX, CPU. Cached AST probs (527, max over windows), audio embeddings and text embeddings of
  every remapped label in a JSON cache file.
  Harness: `crates/daouchi-ai/examples/tune_thresholds.rs` (`cache` then `sweep`), uses the real `select_tags`.

## Findings
- cos(audio, "this is a sound of <tag>"): AST-confident (p>=0.5) tags: p10/50/90 = 0.06/0.27/0.47; unrelated tags
  (p<0.05): -0.14/0.04/0.23; AUC 0.84. CLAP is a useful veto, not a strong positive signal.
- Initial floor 0.45: 41.5% of files get a tag, only 60% of files with AST max>=0.5, 50% of confident tags retained.
- No floor: 99% coverage but 21% of kept tags are "junk" (AST<0.5 and cos<0.05), 2.6 tags/file.

Sweep (weights 0.4/0.6, 306 files; junk = kept tag with AST<0.5 and cos<0.05; strong_keep = share of AST>=0.5 tags kept):

| floor | coverage | conf-file coverage | tags/file | junk | strong_keep | ref F1 |
|---|---|---|---|---|---|---|
| 0.45 (initial) | 0.42 | 0.60 | 0.59 | 0.000 | 0.50 | 0.20 |
| 0.30 | 0.71 | 0.93 | 1.20 | 0.000 | 0.89 | 0.32 |
| 0.20 | 0.86 | 0.995 | 1.57 | 0.008 | 0.98 | 0.41 |
| **0.18** | 0.87 | 1.00 | 1.63 | 0.008 | 0.99 | 0.42 |
| 0.14 | 0.92 | 1.00 | 1.80 | 0.025 | 0.99 | 0.46 |
| none | 0.99 | 1.00 | 2.64 | 0.214 | 1.00 | 0.55 |

Other weights (0.5/0.5 ... 0.8/0.2) trade junk for coverage along the same curve (at floor 0.18: junk 0.8% at 0.4/0.6 vs 2.6% / 4.0% / 4.5% at 0.5/0.5, 0.6/0.4, 0.7/0.3), so 0.4/0.6 stays the best at equal coverage. Ref F1 is measured against the existing suggested tags (noisy; it rises as the floor falls only
because that reference was not produced with this model).

## Chosen
- `AstConfig.clap_validation_floor`: **0.18** (from 0.45; weights 0.4/0.6, min_ast 0.35, boost 1.3 and top 6 are unchanged).
  Effect: every file with a confident AST label gets >=1 tag; CLAP only drops tags it contradicts.
- `SFX_THRESHOLD`: **0.22** (from 0.25). Cosine to the 5 fixed prompts over 1506 library samples:
  p95 = riser 0.37, whoosh 0.40, impact 0.37, atmosphere 0.25, foley 0.19 (single threshold starves atmosphere/foley).
  Counts per 1506 (0.25 / 0.22): atmosphere 76/97, foley 37/52, impact 174/213, riser 210/262, whoosh 188/228.
  Eyeballing the 0.20-0.25 band: atmosphere/foley fine, riser/whoosh noisy, hence a modest drop only.
  Not validated against labels. A per-category rule (z-score >= 1.5 over library, ~6-9% each) would balance categories
  but needs code in `sfx_counts_with`; left as follow-up.

## Regression
`crates/daouchi-ai/tests/tune_regression.rs` + `tests/fixtures/tune_cases.json` (4 real cases: AST probs and CLAP cosines)
pin the tags produced by the defaults; a floor of 0.45 yields 4 tags total against 10 at 0.18.
Re-run the tuning after any CLAP/AST checkpoint change.
