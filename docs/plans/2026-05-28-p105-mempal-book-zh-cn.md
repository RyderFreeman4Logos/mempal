# P105 mempal Book zh-CN

Spec: `specs/p105-mempal-book-zh-cn.spec.md`

> Writing skills used: `makepad:agent-spec-authoring`, `makepad:tech-writer`,
> and `superpowers:writing-plans`.

## Tasks

- [x] Define the P105 task contract and implementation plan.
- [x] Create `books/zh-CN` mdBook structure.
- [x] Land the Chinese outline in `SUMMARY.md`.
- [x] Create one agent-spec writing contract per preface/chapter/appendix unit.
- [x] Create one concrete writing plan per preface/chapter/appendix unit.
- [x] Configure local Mermaid JS assets for rendered diagrams, avoiding the
      installed `mdbook-mermaid 0.17.0` preprocessor because it is incompatible
      with local `mdbook 0.4.52` input.
- [x] Expand the Chinese manuscript from outline notes into source-backed
      technical-book prose covering decisions, architecture, usage, and limits.
- [x] Ignore mdBook generated output under `books/*/book/`.
- [x] Update AGENTS / CLAUDE inventory through P105.

## Chapter Workflow

For each unit under `books/zh-CN/src/`:

- [x] Write a matching task contract under `books/zh-CN/specs/`.
- [x] Write a matching plan under `books/zh-CN/plans/`.
- [x] Anchor factual claims to existing design docs, runbooks, specs, or
      repository inventory.
- [x] Use concise book-chapter structure: positioning, problem, implementation
      boundary, operator usage, and summary.
- [x] Avoid English/Japanese translation work in P105.

## Verification

```bash
agent-spec parse specs/p105-mempal-book-zh-cn.spec.md
agent-spec lint specs/p105-mempal-book-zh-cn.spec.md --min-score 0.7
for f in books/zh-CN/specs/*.spec.md; do agent-spec parse "$f"; done
for f in books/zh-CN/specs/*.spec.md; do agent-spec lint "$f" --min-score 0.7; done
test -f books/zh-CN/book.toml
test -f books/zh-CN/src/SUMMARY.md
test "$(find books/zh-CN/specs -name '*.spec.md' | wc -l)" -ge 12
test "$(find books/zh-CN/plans -name '*.plan.md' | wc -l)" -ge 12
rg "additional-js" books/zh-CN/book.toml
rg "language-mermaid|mermaid.run|mermaid.init" books/zh-CN/mermaid-init.js
test -f books/zh-CN/mermaid.min.js
test -f books/zh-CN/mermaid-init.js
test "$(wc -m books/zh-CN/src/*.md | tail -1 | awk '{print $1}')" -ge 24000
mdbook build books/zh-CN
```
