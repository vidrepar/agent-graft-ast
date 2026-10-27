# Task benchmark results

Model: claude-sonnet-5-5. Runs per arm are listed as pass x/n. Baseline = built-in tools only; ctxonly = built-ins + agent-graft-ast MCP without rename/change_signature; treatment = built-ins + the full agent-graft-ast MCP. Input tokens = input + cache_creation + cache_read. Index build time is excluded.

## ra-add-field-goto-impl (rust-analyzer)

| metric (mean) | baseline | ctxonly | treatment |
|---|---|---|---|
| pass rate | 3/3 | 3/3 | 3/3 |
| wall time | 16s | 23s (+41%) | 7s (-56%) |
| duration_ms | 16s | 22s (+42%) | 6s (-60%) |
| turns | 10.7 | 9.3 (-13%) | 2.0 (-81%) |
| tool calls | 9.7 | 8.3 (-14%) | 1.0 (-90%) |
| input tokens | 28,022 | 36,267 (+29%) | 16,034 (-43%) |
| output tokens | 1,825 | 1,731 (-5%) | 337 (-82%) |
| cost | $0.034 | $0.039 (+15%) | $0.008 (-77%) |
| &nbsp;&nbsp;input_tokens | 8.0 | 8.7 | 4.0 |
| &nbsp;&nbsp;cache_creation_input_tokens | 3,344 | 4,704 | 780 |
| &nbsp;&nbsp;cache_read_input_tokens | 24,670 | 31,554 | 15,250 |
| tools used (all runs) | Edit 15, Read 11, Grep 3 | Edit 15, Read 4, mcp__agent-graft-ast__context 3, Grep 3 | mcp__agent-graft-ast__add_field 3 |

## ra-invert-flip-flag (rust-analyzer)

| metric (mean) | baseline | ctxonly | treatment |
|---|---|---|---|
| pass rate | 3/3 | 3/3 | 3/3 |
| wall time | 33s | 22s (-34%) | 26s (-23%) |
| duration_ms | 33s | 21s (-35%) | 25s (-24%) |
| turns | 11.3 | 13.3 (+18%) | 8.0 (-29%) |
| tool calls | 10.3 | 12.3 (+19%) | 7.0 (-32%) |
| input tokens | 36,163 | 39,221 (+8%) | 53,136 (+47%) |
| output tokens | 3,400 | 3,244 (-5%) | 2,107 (-38%) |
| cost | $0.064 | $0.061 (-5%) | $0.045 (-29%) |
| &nbsp;&nbsp;input_tokens | 8.7 | 8.0 | 10.0 |
| &nbsp;&nbsp;cache_creation_input_tokens | 6,708 | 6,257 | 4,901 |
| &nbsp;&nbsp;cache_read_input_tokens | 29,446 | 32,956 | 48,226 |
| tools used (all runs) | Edit 24, Read 4, Grep 3 | Edit 28, mcp__agent-graft-ast__context 6, Grep 3 | Edit 8, mcp__agent-graft-ast__context 6, mcp__agent-graft-ast__change_signature 4, Read 3 |

## ra-rename-find-struct-impl (rust-analyzer)

| metric (mean) | baseline | ctxonly | treatment |
|---|---|---|---|
| pass rate | 3/3 | 3/3 | 3/3 |
| wall time | 30s | 37s (+22%) | 9s (-69%) |
| duration_ms | 30s | 36s (+22%) | 8s (-71%) |
| turns | 17.7 | 16.3 (-8%) | 3.0 (-83%) |
| tool calls | 16.7 | 15.3 (-8%) | 2.0 (-88%) |
| input tokens | 39,685 | 45,068 (+14%) | 24,525 (-38%) |
| output tokens | 2,980 | 2,813 (-6%) | 452 (-85%) |
| cost | $0.057 | $0.062 (+9%) | $0.019 (-66%) |
| &nbsp;&nbsp;input_tokens | 9.3 | 10.0 | 6.0 |
| &nbsp;&nbsp;cache_creation_input_tokens | 5,945 | 7,539 | 3,117 |
| &nbsp;&nbsp;cache_read_input_tokens | 33,730 | 37,519 | 21,402 |
| tools used (all runs) | Edit 24, Read 21, Grep 5 | Edit 31, Read 7, Grep 4, mcp__agent-graft-ast__context 3, mcp__agent-graft-ast__batch 1 | mcp__agent-graft-ast__rename 3, Grep 3 |

## ts-monorepo-add-param (ts-monorepo)

| metric (mean) | baseline | ctxonly | treatment |
|---|---|---|---|
| pass rate | 3/3 | 3/3 | 3/3 |
| wall time | 21s | 24s (+17%) | 11s (-47%) |
| duration_ms | 20s | 23s (+17%) | 10s (-49%) |
| turns | 11.3 | 14.3 (+26%) | 4.0 (-65%) |
| tool calls | 10.3 | 13.3 (+29%) | 3.0 (-71%) |
| input tokens | 33,201 | 45,792 (+38%) | 28,536 (-14%) |
| output tokens | 2,027 | 2,642 (+30%) | 690 (-66%) |
| cost | $0.039 | $0.058 (+47%) | $0.017 (-57%) |
| &nbsp;&nbsp;input_tokens | 9.3 | 10.0 | 6.7 |
| &nbsp;&nbsp;cache_creation_input_tokens | 3,943 | 6,797 | 1,812 |
| &nbsp;&nbsp;cache_read_input_tokens | 29,249 | 38,985 | 26,717 |
| tools used (all runs) | Edit 21, Grep 5, Read 3, Glob 2 | Edit 21, Read 13, mcp__agent-graft-ast__context 3, mcp__agent-graft-ast__batch 3 | mcp__agent-graft-ast__change_signature 3, mcp__agent-graft-ast__context 3, Edit 3 |

## ts-monorepo-rename (ts-monorepo)

| metric (mean) | baseline | ctxonly | treatment |
|---|---|---|---|
| pass rate | 3/3 | 3/3 | 3/3 |
| wall time | 31s | 43s (+38%) | 13s (-59%) |
| duration_ms | 30s | 42s (+38%) | 12s (-61%) |
| turns | 31.3 | 29.3 (-6%) | 3.0 (-90%) |
| tool calls | 30.3 | 28.3 (-7%) | 2.0 (-93%) |
| input tokens | 53,747 | 63,494 (+18%) | 24,713 (-54%) |
| output tokens | 5,209 | 4,856 (-7%) | 502 (-90%) |
| cost | $0.103 | $0.096 (-7%) | $0.011 (-89%) |
| &nbsp;&nbsp;input_tokens | 9.3 | 11.3 | 6.0 |
| &nbsp;&nbsp;cache_creation_input_tokens | 11,800 | 10,524 | 1,031 |
| &nbsp;&nbsp;cache_read_input_tokens | 41,937 | 52,958 | 23,676 |
| tools used (all runs) | Edit 56, Read 30, Grep 5 | Edit 48, Read 28, Grep 5, mcp__agent-graft-ast__context 3, mcp__agent-graft-ast__batch 1 | mcp__agent-graft-ast__rename 3, Grep 3 |

## ts-service-add-param (ts-service)

| metric (mean) | baseline | ctxonly | treatment |
|---|---|---|---|
| pass rate | 3/3 | 3/3 | 3/3 |
| wall time | 19s | 18s (-1%) | 12s (-33%) |
| duration_ms | 18s | 18s (-2%) | 12s (-35%) |
| turns | 11.0 | 14.7 (+33%) | 5.0 (-55%) |
| tool calls | 10.0 | 13.7 (+37%) | 4.0 (-60%) |
| input tokens | 26,634 | 32,529 (+22%) | 26,183 (-2%) |
| output tokens | 2,078 | 2,504 (+20%) | 890 (-57%) |
| cost | $0.043 | $0.046 (+6%) | $0.019 (-55%) |
| &nbsp;&nbsp;input_tokens | 6.0 | 8.0 | 6.0 |
| &nbsp;&nbsp;cache_creation_input_tokens | 5,075 | 4,465 | 2,050 |
| &nbsp;&nbsp;cache_read_input_tokens | 21,553 | 28,056 | 24,127 |
| tools used (all runs) | Edit 24, Read 3, Grep 3 | Edit 24, Read 11, Grep 3, mcp__agent-graft-ast__context 3 | mcp__agent-graft-ast__change_signature 3, mcp__agent-graft-ast__context 3, Edit 3, Grep 3 |

## Totals (all tasks)

| metric (mean) | baseline | ctxonly | treatment |
|---|---|---|---|
| pass rate | 18/18 | 18/18 | 18/18 |
| wall time | 25s | 28s (+11%) | 13s (-48%) |
| duration_ms | 24s | 27s (+11%) | 12s (-50%) |
| turns | 15.6 | 16.2 (+4%) | 4.2 (-73%) |
| tool calls | 14.6 | 15.2 (+5%) | 3.2 (-78%) |
| input tokens | 36,242 | 43,729 (+21%) | 28,854 (-20%) |
| output tokens | 2,920 | 2,965 (+2%) | 830 (-72%) |
| cost | $0.057 | $0.060 (+6%) | $0.020 (-65%) |
| &nbsp;&nbsp;input_tokens | 8.4 | 9.3 | 6.4 |
| &nbsp;&nbsp;cache_creation_input_tokens | 6,136 | 6,714 | 2,282 |
| &nbsp;&nbsp;cache_read_input_tokens | 30,097 | 37,005 | 26,566 |
| tools used (all runs) | Edit 164, Read 72, Grep 24, Glob 2 | Edit 167, Read 63, mcp__agent-graft-ast__context 21, Grep 18, mcp__agent-graft-ast__batch 5 | Edit 14, mcp__agent-graft-ast__context 12, mcp__agent-graft-ast__change_signature 10, Grep 9, mcp__agent-graft-ast__rename 6, mcp__agent-graft-ast__add_field 3, Read 3 |
