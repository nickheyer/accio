# accio
EZ anthropic account switcheroo. No slop features, no dashboard, just accio &lt;your other account>.

```
accio                                open the TUI
accio <your other account>           make account the live account
accio list                           list accounts
accio add [your other account]       add currently logged in account or log one in if specified
accio remove <your other account>    remove an account from accio
```

## Install

```
cargo install --path .
```

## Stuff

There is almost nothing here:

- We are just doing the same thing that claude already does
- Accounts are backed up and restored from at `~/.config/accio/accounts/<your other account>.json` (linux) or `~/Library/Application Support/accio/accounts` (mac), sorry windows hope for the best
- Accio gets usage from CC `/usage`
- Basically just minimizes reauth requirements and lets you switch based on usage
- `CLAUDE_CONFIG_DIR` is used if set
- Hot reload without downtime like ai god intended... is not yet available. Anthropic pls.

