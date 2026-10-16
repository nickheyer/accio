# accio
EZ ai credentials/config switcheroo. No slop features, no dashboard, just accio &lt;your other account>.

```
  accio                                           open the TUI
  accio <provider> [--profile NAME] [--] [args]   launch as session
  accio <your other account>                      set live account
  accio list                                      list accounts
  accio add [provider] [name]                     add currently logged in account or log one in if specified
  accio configure <provider> [name] KEY=VALUE...  for advance profile configuration
  accio remove <your other account>               remove an account from accio
  accio provider list                             list built-in and custom providers
  accio provider add <name> [command]             create a custom provider
  accio provider remove <name>                    drop a custom provider
  accio provider enable|disable <name>            show or hide any provider
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
- Accounts live at `~/.config/accio/accounts/<provider>/<your other account>.json` (linux) or `~/Library/Application Support/accio/accounts` (mac), sorry windows hope for the best
- Basically just minimizes reauth requirements and lets you switch based on usage
- Hot reload without downtime like ai god intended.
- All major harnesses supported (claude code, grok, codex, gemini)
- Custom profiles for more advanced harness configurations
- Custom providers for any other harness, described in one json file

## Custom Configs

Point a harness at any inference provider without logging in:

```
accio configure claude zai ANTHROPIC_BASE_URL=https://api.z.ai/api/anthropic ANTHROPIC_AUTH_TOKEN=sk-...
accio zai
```

## Custom providers

Any harness with a config directory can be a provider without touching accio's code. Relevant configs can be found here: `~/.config/accio/providers/<name>.json` or `~/Library/Application Support/accio/providers`

```json
{
  "command": "pi",
  "home": "~/.pi/agent",
  "home_env": "PI_CODING_AGENT_DIR",
  "copy": ["settings.json", "auth.json", "models.json", "extensions", "skills"],
  "knobs": [
    { "name": "PROVIDER", "hint": "provider id pi lists the models under" },
    { "name": "API_KEY", "hint": "bearer token for that endpoint", "secret": true }
  ],
  "files": {
    "merge:models.json": {
      "providers": {
        "${PROVIDER}": { "baseUrl": "${BASE_URL}", "api": "${API}", "apiKey": "${API_KEY}",
                         "models": [{ "id": "${MODEL}", "contextWindow": "${CONTEXT_WINDOW:json}" }] }
      }
    },
    "merge:settings.json": { "defaultProvider": "${PROVIDER}", "defaultModel": "${MODEL}" }
  }
}
```

- `command` is the program to run, or an array with its arguments
- `home` is the harness's config directory, `home_env` the variable that relocates it
- `copy` names what a session takes along from the live home
- `files` are templates written under home, `merge:` ones deep merge into existing json, the rest are written whole
- `${KNOB}` fills in a profile value, `${KNOB:json}` keeps numbers and booleans typed, an unset knob drops its leaf
- `knobs` adds hints and marks secrets, anything a template mentions is a knob already
- `env` sets extra session variables, `unset` scrubs inherited ones, `"disabled": true` hides the provider

Profiles are then ordinary configured profiles:

```sh
accio provider add pi --preset pi
accio configure pi nebu PROVIDER=nebu BASE_URL=http://nebu:8484/v1 API=openai-completions API_KEY=... MODEL=freddie
accio nebu                      # merges into the live ~/.pi/agent, lifts out on the next switch
accio pi --profile nebu         # private copy of ~/.pi/agent with the profile laid over it
```

In a session every knob is also exported as an environment variable, so a harness that reads `$API_KEY` style references gets them without a template. Built ins can only be switched off: `accio provider disable claude`.

## Isolated sessions

Launch a harness with a saved profile without switching its default account:

```sh
accio claude
accio codex
accio gemini
accio grok
```

Skip the picker with `--profile`, and pass arguments to the harness after `--`:

```sh
accio claude --profile work
accio claude --profile personal -- -p "are you sentient"
accio codex --profile work -- exec "Run the tests"
accio gemini --profile personal -- -p "why is gemini so bad"
accio grok --profile work
```
