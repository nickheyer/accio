# accio
EZ ai credentials/config switcheroo. No slop features, no dashboard, just accio &lt;your other account>.

```
  accio                                           open the TUI
  accio <your other account>                      make account the live account (provider/name if ambiguous)
  accio list                                      list accounts
  accio add [provider] [name]                     add currently logged in account or log one in if specified
  accio configure <provider> [name] KEY=VALUE...  for advance profile configuration
  accio remove <your other account>               remove an account from accio
  accio help                                      show this help"
```

## Install

With cargo (any os):

```sh
cargo install --git https://github.com/nickheyer/accio accio
```

On Arch:

```sh
yay -S accio
```

## Stuff

There is almost nothing here:

- We are just doing the same thing that claude already does
- Accounts are backed up and restored from at `~/.config/accio/accounts/<your other account>.json` (linux) or `~/Library/Application Support/accio/accounts` (mac), sorry windows hope for the best
- Basically just minimizes reauth requirements and lets you switch based on usage
- Hot reload without downtime like ai god intended.
- All major harnesses supported (claude code, grok, codex, gemini)
- Custom profiles for more advanced harness configurations

## Custom Configs

Point a harness at any inference provider without logging in:

```
accio configure claude zai ANTHROPIC_BASE_URL=https://api.z.ai/api/anthropic ANTHROPIC_AUTH_TOKEN=sk-...
accio zai
```

