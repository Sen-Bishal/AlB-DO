# pulse-dashboard

Phase J demo. Exercises every JSX expression shape the renderer used to
silently drop, plus the new shell-stamping and static-slicer dedup
contract.

What it proves:

- `{count}` from `useState(0)` renders `0` (not an empty span). Phase K
  will make the button actually increment; Phase J just makes the read
  correct.
- `{iso}` from `new Date(...).toISOString()` renders a real ISO string.
- Status pills display `(ratio * 100).toFixed(1) + "%"` — exercises
  arithmetic, method calls, and string concatenation.
- `data-albedo-id="<u32>"` is stamped on every host element so bakabox
  can address them when patches start arriving.

Render with:

```
albedo dev --root examples/pulse-dashboard
```

Or as a one-shot production server:

```
albedo serve   # build + serve via the same stitcher as dev
```

The end-to-end render is asserted in `tests/pulse_dashboard_demo.rs`.
