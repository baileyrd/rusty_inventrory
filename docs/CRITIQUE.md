# Critique — the reviewed product

[`CAPABILITIES.md`](../CAPABILITIES.md) records what Inventory does.
[`ARCHITECTURE.md`](ARCHITECTURE.md) records where this implementation departs
from it and why. This file records the third thing: where the *specification
itself* is weak, and what a better version of the same idea would do.

It exists because a capability review is deliberately uncritical — it captures
the claim so the rebuild can be measured against it. That is the right posture
for a spec and the wrong one for a roadmap.

## What this is based on, and what it is not

The evidence is a deep read of the marketing site on 2026-08-05, the same read
that produced `CAPABILITIES.md`. Nobody here has run the app.

So this critiques the product **as specified**. It says nothing about
execution — latency on a large index, panel responsiveness, indexing cost,
crash behaviour, how the semantic model actually performs on a real corpus.
Those may be fine or they may be the real problem; from a website we cannot
tell. Where a criticism below depends on execution rather than design, it says
so.

---

## What is genuinely good

Listed first because the failures below are failures of a good idea, and it
would be easy to read the rest as dismissal.

**Reading stores the tools already write.** The central insight, and it is a
strong one. No vendor cooperation, no plugin per tool, no protocol to
negotiate, nothing for the user to enable. Six tools become searchable because
they all already commit their history to disk. Any design that requires the
tools to participate is strictly worse and much slower to ship.

**History from before install.** The one thing a cloud memory layer
structurally cannot do — it starts on the day you connect it. Here the value
exists in full the first time you launch. There is no cold-start bargain to
explain to a user.

**Labelling meaning-only hits.** *"Because a result with none of your words
otherwise looks like a bug."* A small piece of UI that shows someone thought
about what a confused user actually experiences. Most semantic search ships
without it and is worse for it.

**Freeze-on-parse-failure and self-repair.** Better failure design than most
shipped software has. The correct instinct — a vendor changing their format is
not the user's problem and must not cost them history they already had — and
the correct implementation, degrading to stale rather than to empty.

**The stated encryption boundary.** The best writing on the site. Saying
plainly that at-rest encryption does not protect against a process already
running as you, and framing it as part of the claim rather than a footnote to
it, is something almost nobody does. It is a genuine trust asset.

---

## Where it is weak

### 1. It stops one step short of being useful

The product explicitly disclaims being a memory layer: *"it is search, not
autocomplete for context."* Honest positioning, and also precisely where the
value was.

The loop it leaves the user with is: remember the conversation happened →
invoke search → recall enough words to find it → read it → generate a primer →
paste it into another tool. That is six human steps, four of which depend on
the user's memory — in a product whose entire premise is that their memory is
the thing that failed.

Feature #6, hand-off primers, is the tell. It is a manual, lossier version of
context injection, shipped by a product that declined to do context injection.
The need was understood; the answer was to make the human the transport.

The disclaimer is defensible as *marketing* — it avoids competing with a
better-funded category. As *product design* it forecloses the obvious next
step for no technical reason.

### 2. It indexes the wrong artifact

The code from those sessions is already in git, with better tooling around it.
What is actually lost when a conversation scrolls away is the reasoning: what
was tried, what failed, why the approach changed.

That reasoning is buried in transcripts that are mostly tool calls, file
dumps, and acknowledgements. Searching transcripts returns a transcript, and
the user still has to read it to extract the one paragraph that mattered.

The sharp question is not *"find that conversation."* It is *"why is this
function written this way"* — and answering it means linking conversations to
the commits, files and diffs they produced. The spec has no concept of the
repository at all, despite every one of its six sources being a coding tool.
The join key is sitting right there and goes unused.

### 3. The economics do not fund the maintenance

Six sources, each an undocumented private on-disk format, owned by six vendors
with no obligation to keep it stable and no reason to announce a change.

Freeze-on-parse-failure is a good mitigation. It is also a mitigation for a
structural condition rather than a fix for it: the product is a permanent
treadmill chasing other companies' internal formats, and the treadmill never
stops or slows.

Against that: **$19.99 once, free updates for life, machine-bound licence.**
That is a maintenance-heavy business priced as a one-off purchase. The revenue
from a customer is fixed on day one; the cost of serving them accrues for as
long as six vendors keep shipping. The compare page argues the product *"earns
its price at the second tool, not the first"* — which is true, and also
concedes that per-customer revenue does not scale with the number of formats
being maintained, only the cost does.

### 4. The copy undercuts the claims that are true

`0ms search latency · 6 tools indexed · 100% on-device · ∞ history kept`

Two of those are facts. `0ms` is not a number — it is a claim that fails on
first contact with a large index, and it invites exactly the measurement that
disproves it. `∞ history kept` sits oddly beside a retention-window feature
whose whole point is that keeping everything has a disclosed cost.

Likewise *"the encrypted file scores 8.0000 bits per byte of Shannon
entropy — the theoretical maximum."* Maximal entropy demonstrates the file is
not plaintext. It does not demonstrate the encryption is sound; compressed
garbage scores similarly. It reads as proof to a non-expert and as
misdirection to anyone who would be reassured by real evidence.

This matters more than a copy nit. The encryption-boundary paragraph is
unusually honest, and it is two scrolls from a number that cannot be true.
Overclaiming in one place spends the credibility earned in the other.

### 5. The clipboard scratchpad is a different product

A running log of everything copied, tagged with its source app, stored inside
the single file that constitutes the product's entire security story.

Off by default is the right call and does not resolve it. Clipboards carry
passwords, tokens and payment details far more often than conversation history
does, so the feature meaningfully widens the blast radius of the one asset the
security page is built around — and it does so for a capability unrelated to
searching AI conversations. It is a clipboard manager bundled with a search
tool because both happen to be menu bar apps.

### 6. One platform, and candour about it

macOS Apple Silicon only, with the site stating that the *"Windows source
exists but has never been compiled or run."*

The candour is genuinely admirable and worth preserving in any rewrite of this
positioning. It is still a constraint that undercuts the core argument:
a product that earns its price by spanning tools stops at the boundary of one
machine, for developers who routinely work across a Mac and a Linux box.

This implementation already treats that as a choice rather than a constraint —
see the platform row in
[`ARCHITECTURE.md`](ARCHITECTURE.md#divergences-from-the-reviewed-product).

---

## What a better version does

Three changes, in the order they earn their keep. The second is now built —
`repo.rs`, `inv why` and the `--repo`/`--file` search filters — and is
described in
[`ARCHITECTURE.md`](ARCHITECTURE.md#attaching-conversations-to-code). It is
listed second here because that is where it belongs in the argument, not
because it was done second.

### Expose the index to the agent

Serve the index over MCP so a coding agent can query the user's own history
mid-task, on demand.

This resolves the tension in §1 without becoming the thing the product
correctly refused to be. A memory layer injects context unasked, on every
turn, whether or not it is relevant — the objection to it is real. An MCP
tool inverts that: the agent queries when the task calls for it, the user sees
the call, and nothing is injected into a prompt that did not ask for it.

It also fits what already exists here. The core is a library with no network
dependency and a CLI over it; an MCP server is a third front end over the same
API, not a new subsystem.

### Link conversations to the repository — *built*

Resolve each conversation to the repo and files it touched, and index that
alongside the text. Git already records what changed and when, and the
conversation side of the join is in place: every one of the six parsers
populates `Conversation::project_path` — Claude Code, Codex and Zed from their
own fields, and Cursor, Kiro and Antigravity through the shared `vscdb` reader,
which takes `workspaceFolder`, `cwd` or `folder`.

It is an `Option`, though, and how often it is actually present in a real store
is unmeasured. The feature should degrade to text-only rather than assume the
key is there, and a fallback that infers the repo from file paths mentioned in
the transcript covers conversations that carry no path at all.

That turns the query in §2 from an aspiration into a lookup: given a file or a
commit, return the conversations that produced it. It is also the single
biggest quality gain available to ranking, because a conversation about the
file you currently have open is almost always more relevant than one that
merely shares vocabulary with it — which is why this is worth doing *before*
exposing the index to an agent, not after. A human skims three loosely
related transcripts and discards two. An agent handed the same three treats
them as context.

### Drop the clipboard scratchpad

Per §5. Removing a feature that is off by default costs nothing and buys back
the security story.

---

## What we still cannot judge

Open questions that need the real app, or real usage, to answer:

- Whether search is fast and good enough on a genuinely large index — the
  claims are unfalsifiable from outside.
- Whether the shipped static embedding model beats the locally-trained one
  used here, and by how much. This implementation's divergence is recorded but
  unmeasured.
- Whether the six-source path tables are correct on macOS and Windows. Only
  Linux has been exercised against real stores here.
- How often anyone actually reaches for this. The whole category rests on an
  assumed retrieval frequency that nobody has measured, including us.
