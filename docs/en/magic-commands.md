# Magic Commands Reference

Magic commands are slash commands you type in the TUI input area. They execute immediately without going through the LLM.

Type `/` to see the command palette with fuzzy search.

## Session Management

### `/new [name]`
Create a new session. Optionally provide a name for easy identification.

```
/new my-project
```

### `/session [id]` — alias: `/ss`
Switch to an existing session. Without arguments, lists all sessions.

```
/ss                    # list all sessions
/ss abc123             # switch to session abc123
```

### `/fork <uuid>`
Create a new session forked from a specific message. The new session contains all messages up to and including the specified message.

```
/fork a1b2c3d4         # fork from message with UUID a1b2c3d4
```

### `/rewind [uuid|list]` — alias: `/rw`
Rewind the current session to a specific user message. All messages after that point are discarded.

```
/rw list               # list rewritable messages
/rw a1b2c3d4           # rewind to this message
```

## Model & Agent

### `/model [name]` — alias: `/m`
View or switch the model. Without arguments, shows the current model and triggers the model selection popup.

```
/model                 # show model selection popup
/model gpt-4           # switch to gpt-4
```

### `/agents [name]`
View or switch the agent template. Agent templates define the system prompt, tools, and model.

```
/agents                # list available templates
/agents coder          # switch to "coder" template
```

### `/think [on|off|low|medium|high|xhigh|max]` — alias: `/t`
Control reasoning/thinking mode.

```
/think off             # disable thinking
/think high            # set reasoning effort to high
/think                 # toggle thinking on/off
```

## Context & Compression

### `/compact` — alias: `/cp`
Manually trigger context compression. The compactor summarizes old messages and keeps recent ones intact. Useful when approaching the context window limit.

```
/compact
```

### `/context` — alias: `/ctx`
Display context statistics: message count, token usage, and the current system prompt.

```
/ctx
```

## Execution Control

### `/interrupt` — alias: `/int`
Interrupt the current agent turn. Clears the agent's inbox and resets the event loop. The agent stops processing immediately.

```
/int
```

### `/yolo [on|off]`
Toggle YOLO mode. When enabled, all bash commands skip the safety confirmation and execute immediately. You can also enable yolo per-session by selecting the `yolo` option in the safety prompt.

```
/yolo on               # enable (dangerous!)
/yolo off              # disable (safe, default)
```

## Direct Execution

### `/bash <command>` — aliases: `/b`, `/sh`
Execute a shell command directly without going through the agent. Output is displayed in the chat.

```
/bash ls -la
/b git status
/sh cat package.json
```

## Information

### `/help` — aliases: `/h`, `/?`
Show all available magic commands with descriptions.

### `/skills`
List all loaded skill files and their paths.

### `/reload`
Reload configuration, hooks, magic commands, skills, and rules from disk. Useful after editing config files without restarting the gateway.

```
/reload
```

## Prompt Commands

In addition to the built-in commands above, wing supports **prompt-type commands** — custom commands defined in Markdown files. These are loaded from paths specified in config:

```yaml
# ~/.wing/config.yaml
commands:
  paths:
    - "~/.wing/commands/*.md"
```

Each `.md` file defines a command. The file content becomes the prompt sent to the agent when the command is invoked. See the skills/rules system for the file format.
