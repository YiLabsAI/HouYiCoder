---
name: skillify
description: Capture this session's repeatable process into a reusable skill
allowed-tools: ["Read", "Write", "Edit", "Glob", "Grep", "AskUserQuestion", "Bash(mkdir:*)"]
when_to_use: Use at the end of a workflow you want to capture as a reusable skill. Call with a short description of the process.
argument-hint: "[description of the process you want to capture]"
disable-model-invocation: true
user-invocable: true
---

# Skillify: Capture a Workflow into a Skill

You are capturing the current session's repeatable process into a reusable skill. Follow these steps exactly.

## Step 1: Analyze the Session

The user described this process as: {{userDescription}}

Review the conversation history and the user messages below. Identify:
- The repeatable process (what the user was doing)
- Inputs and parameters (what varies between runs)
- Ordered steps (the sequence of actions)
- Success artifacts and criteria (what "done" looks like)
- Where the user corrected you (these reveal important constraints)
- Tools, permissions, and agents used

User messages from this session:
{{userMessages}}

Session memory:
{{sessionMemory}}

## Step 2: Interview the User

Use AskUserQuestion (never plain-text questions) for each round:

**Round 1**: Confirm the skill name and description. Ask about high-level goals and success criteria.

**Round 2**: Present the numbered steps you identified. Suggest arguments if any. Ask where to save:
- "This project" — `.houyicoder/skills/<name>/SKILL.md`
- "Personal" — `~/.houyicoder/skills/<name>/SKILL.md`

Do not add your own "needs adjustment" option; the user always has "Other" for free-text input.

Ask whether the skill should run inline or forked (forked = sub-agent, better for self-contained tasks).

**Round 3**: For each step, confirm: what artifacts it produces, success criteria, human checkpoints (especially for irreversible actions like merging or messaging), whether it can run in parallel, and execution style. Pay special attention to places where the user corrected you during the session — these reveal important constraints. Iterate one round per step if the process is complex.

**Round 4**: Confirm invocation triggers — what phrases or situations should prompt this skill. Collect any gotchas.

Do not over-ask for simple processes. If the process is straightforward, compress rounds.

## Step 3: Write the SKILL.md

Create the skill directory and write SKILL.md at the user-chosen location. Follow this structure:

```markdown
---
name: <skill-name>
description: <one-line description>
allowed-tools: ["Read", "Write", "Bash(git:*)"]
when_to_use: Use when <trigger conditions>
argument-hint: "[args description]"
---

# <Title>

## Inputs
<what the skill takes as input>

## Goal
<what success looks like>

## Steps

### 1. <Step Name>
<instructions>
**Success criteria**: <how to know this step is done>
**Artifacts**: <files or state this step produces, if any>
**Human checkpoint**: <confirmation needed before irreversible actions, if any>

### 2. <Step Name>
<instructions>
**Success criteria**: <how to know this step is done>
**Artifacts**: <files or state this step produces, if any>
**Human checkpoint**: <confirmation needed before irreversible actions, if any>
```

Frontmatter rules:
- `name` must be lowercase, hyphens only, no spaces
- `allowed-tools` should use scoped patterns where possible (e.g. `Bash(git:*)` not bare `Bash`)
- `when_to_use` should describe trigger phrases or situations clearly — this is how the model decides when to invoke the skill
- Keep descriptions concise; the listing only shows the first line

## Step 4: Confirm Before Saving

Before writing the file, output the complete SKILL.md content as a code block in your response so the user can review it. Then use AskUserQuestion to ask: "Does this skill look good to save?"

Only after the user confirms, write the file. After writing, report:
- The saved path
- The invocation: `@skill:<name> [arguments]`
- That the user can edit the SKILL.md directly to refine it
