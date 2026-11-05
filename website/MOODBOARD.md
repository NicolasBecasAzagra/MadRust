# Moodboard

The site has to read as a project of systems engineering, not as a generated
landing page. These are the references, and exactly what is taken from each.

The direction is a **serial console / lab notebook**: monospace first, one
accent, hairline rules instead of cards and shadows, the real boot log and the
real numbers as the protagonists.

## References

1. **Ghostty — https://ghostty.org/**
   Mono-first interface: JetBrains Mono for the whole UI and every snippet, a
   humanist face only for prose. One claim in the hero, everything else a
   verifiable fact. Documentation treated as the product, with snippets that
   can be copied literally.
   *Borrowed:* mono as the primary voice of the site; the hero claim is a
   measured number, not an adjective; every code block is copyable and real.

2. **Oxide Computer — https://oxide.computer/**
   Two families with different jobs: a text face for prose, a mono only for
   labels, figures and captions. A measured type scale (roughly 1.12–1.25 per
   step) with no size outside the scale. Console captures are numbered "Fig. N"
   and the text refers to them.
   *Borrowed:* the mono carries labels, figures and data, not paragraphs; a
   hard type scale with explicit steps; figures referenced from the prose
   instead of floating as decoration.

3. **Redox OS — https://www.redox-os.org/**
   The home page of an operating system reads as a release note: version,
   paragraph, then sections with a bold lead-in and bullets. The design budget
   is spent on content, not on chrome.
   *Borrowed:* the roadmap and the changelog are presented as engineering
   records with lead-ins and bullets, not as marketing sections.

4. **Zed — https://zed.dev/**
   The hero is the real product, not an illustration of the product. One
   section per thing, one sentence each, one call to action.
   *Borrowed:* the hero shows the real boot log of the real kernel; sections
   earn their place with a fact, not with a paragraph.

5. **Linear — https://linear.app/**
   High density, 1 px separators, small radii, a single accent per view. Mono
   is reserved for identifiers and metadata. Motion only as a response to an
   action.
   *Borrowed:* dense technical zones (tables, commands, changelog) against
   airy explanatory zones; no fade-up on every section; one accent that marks
   state or action.

6. **Fly.io — https://fly.io/**
   Three typographic roles (display, interface, code) and a single accent used
   for action and state, never as decoration. Documentation density: short
   paragraph, snippet, table.
   *Borrowed:* the typographic role split; figures with tabular numerals; the
   rhythm of short prose followed by evidence.

7. **Rust — https://www.rust-lang.org/**
   One display face and one text face. Few colors, all of them semantic.
   Hierarchy by size and weight, not by hue.
   *Borrowed:* a single accent that means one thing; hierarchy carried by type
   and rules rather than by color.

8. **Tailwind CSS theme documentation — https://tailwindcss.com/docs/theme**
   Tokens as an API: if a value is not a token, it does not exist. Spacing on a
   0.25 rem base, line-height paired with size, primitives separated from
   semantics.
   *Borrowed:* the token architecture of the new stylesheet: primitives,
   semantics, then components; a spacing base; no stray hex values.

## Patterns taken from the gallery search

"developer tool landing page", "terminal aesthetic", "brutalist technical site"
across Awwwards, recent.design, Mobbin and Dribbble produced five patterns
worth building, none of them copied:

1. **Terminal window as a frame for code.** A caption bar with the file name on
   the left and a copy button on the right, a 1 px border, no shadow.
2. **Monospace metadata rail with real numbering.** A two column grid, index on
   the left in mono, content on the right, separated by a hairline. The
   numbering is a sequence that exists, not decoration.
3. **Hairline grid, zero radius, no shadows.** A container with a 1 px border,
   cells separated by borders, flat background. The grid is the decoration.
4. **Sticky specification sidebar with scroll spy.** A narrow mono column of
   anchors on the left, wide content on the right; on mobile it collapses into
   a disclosure.
5. **One reveal, on the chart.** A single observer animates the benchmark
   chart when it enters the viewport, not every section on the page.

## The benchmark chart: a deliberate deviation

The original brief asked for "horizontal bars on a log scale". That is the one
instruction this project does not follow, because it would publish a chart the
project itself would call dishonest.

The measurements span 376 ns to 44.9 ms, about five orders of magnitude, so a
linear scale cannot show them. On a logarithmic scale a solid bar has no
meaningful zero: its length no longer encodes the value, and the bar starts
somewhere arbitrary. Datawrapper and GraphPad both reject bar charts on log
scales for exactly that reason.

Instead: a **dot plot with stems** on a log axis, where position carries the
magnitude, every value is labelled directly with its unit, each row carries
one ratio annotation, and the axis states that zero is not representable. The
raw table stays available underneath. The same argument is the project's own:
every loss is published, and the measurement is never dressed up.
