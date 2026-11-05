---
name: agentctl-chat
description: Answer chat requests that the agentctl chat bridge types into your pane, and check what reached the chat. Use when a prompt says "The user's request arrived through the configured chat bridge", when you need to reply to a chat user, or when you need to confirm that a reply was sent.
---

# agentctl chat bridge: answering requests

The bridge (`agentctl chat run`) types each chat message from an allowed sender
into the coordinator's pane as a request prompt, and sends your replies back to
the chat thread.

This skill describes the compiled (Rust) bridge, whose prompts say "The user's
request arrived through the configured chat bridge" and open with the message's
send time. The Python edition (`py/bin/agentctl chat`) words its prompts
differently, has no `sent` or `thread` command (it has `chat context --state`)
and adds no send-time line or delivery ✅; follow its own `--help` and the
instructions in the prompt it types. When a prompt prints a command, run it as
printed: it normally starts with the absolute path of the service's own
executable, so it is the right edition. A `chat thread` command that starts
with plain `agentctl` (printed when that executable's file is gone) needs the
compiled edition to be the `agentctl` on `PATH`.

For the compiled bridge, use the installed command as the authority:

- `agentctl chat quickstart`
- `agentctl chat userguide`
- `agentctl chat sent --bridge-state DIR`
- `agentctl chat thread --bridge-state DIR --thread THREAD --last 20`
- `agentctl chat reply --help`

## Reply with the two marker lines

The prompt gives a reply ID and two marker lines:

```text
<CHAT_REPLY_007>
your answer, as many lines as you need
</CHAT_REPLY_007>
```

**This is the normal way to reply.** Put the opening line at the start of your
answer and the closing line after it, in your ordinary turn output. The text
between them reaches the user twice: in this terminal, where the owner may be
reading, and in the chat thread. Use the same two lines for every reply to that
request, including short progress updates during a long task. Each block with
new text is sent as one chat message; a block whose text the request has
already sent is not sent again, so give each progress update new wording. Make no tool call between the two lines. If the prompt
gave a numbered ID instead (`<CHAT_REPLY_<nonce>_1>`), add one to the number for
each later reply, as the prompt says.

## The reply command is the exception

When the bridge runs with `--offer-reply-command`, the prompt also prints an
exact `agentctl chat reply ... --file PATH_TO_YOUR_REPLY` command. Use it only
when the marker lines cannot do the job:

- you must send something **before your turn ends**, and the text would not
  otherwise appear until then; or
- a block you wrote between the two lines did not reach the chat: the bridge
  reported it as not sent (a short notice in your pane, beginning `Chat reply
  not sent`), or `agentctl chat sent` does not list it, as when the screen was
  redrawn before the bridge read it.

Send each reply one way only. A reply sent by the command does not appear in
this terminal by itself; printing a copy without the marker lines sends nothing,
so it is a safe way to show the owner what you sent.

## Check what reached the chat

`agentctl chat sent --bridge-state DIR` lists the replies the bridge holds,
newest first: request, reply number, phase, the provider's message ID once it is
sent, and when it was captured and sent. Phase `sent` with a provider message ID
means the chat provider accepted the message. `pending` means captured and not
yet sent; `sending` means a send is in progress or its outcome is unknown, and
the bridge retries it with the same operation ID, so it is never posted twice.
`agentctl chat thread` shows one thread's retained requests and replies in
order. A retired request's replies are no longer held, so the chat thread
itself remains the complete record. The state directory is the `--bridge-state`
argument of the prompt's `chat thread` command (printed for a reply in an
existing thread) or its `chat reply` command (printed when the bridge offers
it); either can be left out when it cannot be printed safely. Otherwise it is
the `--bridge-state` of the service's `agentctl chat run` command, which the
owner or the deployment's notes give.

## What the reactions and the first line mean

- The acknowledgement reaction (configured per deployment, for example 👀) is
  added when the bridge admits the message.
- ✅ is added, when the deployment has an acknowledgement reaction other than
  ✅, once the bridge's queue has typed the prompt into your pane and verified
  it against your composer, printed or queued behind another prompt. Adding it
  can fail or lag like any provider call, so a missing ✅ does not prove the
  prompt was not delivered. An older deployment whose acknowledgement reaction
  is itself ✅ uses it to acknowledge admission instead; it can appear before
  or after the prompt reaches you, and no separate delivery receipt is added.
- Each prompt's first line opens with the message's own send time, as
  `Sent 2026.10.07:08:45 EDT.`, and says `delivered 1 h 12 min later` when it
  reached you two minutes or more after that. Several held messages can arrive
  together after an outage or a long turn; answer them knowing how old they
  are.

## Do not

- Do not use another chat tool or the provider's API to answer a bridged
  request; the bridge tracks replies per request.
- Do not write a request's marker lines around text you do not mean to send,
  such as an example or a quotation: the bridge reads them from your visible
  output.
- Do not answer a bridged request only in the terminal: the chat user sees
  nothing that is not between the two lines or sent by the reply command.
