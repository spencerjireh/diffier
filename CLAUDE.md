# diffier

`main` is protected: every change lands through a squash-merged pull request.

## Workflow

- Start work on a branch cut from `main`, named `<type>/<slug>` where `<type>`
  is a Conventional Commits type: `feat/session-filter`, `fix/spool-race`,
  `docs/design`.
- Before pushing, run the checks listed in [CONTRIBUTING.md](CONTRIBUTING.md).
- Open the PR with `gh pr create --fill-first`. The PR title is a Conventional
  Commits subject: squash merge uses it as the commit subject and the PR body
  as the commit body.
- Merge with `gh pr merge --auto --squash` when the user asks for the merge.
- The job names in `.github/workflows/ci.yml` are required status checks on
  `main`. Renaming a job also means updating the ruleset.
