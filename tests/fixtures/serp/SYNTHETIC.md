# Synthetic SERP fixtures (B01)

- `html-ad.html`: no live sponsored block was captured on 2026-10-30 for the
  chosen query, so this block is synthetic, matching the observed DDG html
  structure (`div.result.result--ad` with an `a.result__a` and an
  `a.result__snippet`); the organic neighbours are synthetic too. It exists to
  make ad exclusion a failing check.
- `lite-ad.html`: synthetic lite sponsored row (`tr.result-sponsored` with the
  `span.result-sponsored` label) between two synthetic organic rows.
- `html-anomaly.html`: synthetic challenge page shaped like DDG's anomaly modal;
  it contains no `div.result` block and no `.no-results` marker, so the parser
  must return a typed failure rather than an empty success.
- The real captures are `html-results.html`, `lite-results.html` and
  `html-empty.html` with their own `.md` provenance files.
