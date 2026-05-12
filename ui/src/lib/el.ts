// Tiny DOM helper. No framework — just a typed `h()` like a stripped-down
// hyperscript. Keeps the bundle small and the call sites legible.

export type Child = Node | string | number | null | undefined | false;

type ElProps<T extends keyof HTMLElementTagNameMap> = Partial<
  Omit<HTMLElementTagNameMap[T], "style" | "children" | "classList">
> & {
  class?: string;
  style?: Partial<CSSStyleDeclaration>;
  on?: Partial<{
    [K in keyof HTMLElementEventMap]: (ev: HTMLElementEventMap[K]) => void;
  }>;
  dataset?: Record<string, string>;
};

export function h<T extends keyof HTMLElementTagNameMap>(
  tag: T,
  props: ElProps<T> | null,
  ...children: Child[]
): HTMLElementTagNameMap[T] {
  const el = document.createElement(tag);
  if (props) {
    for (const [k, v] of Object.entries(props)) {
      if (v == null) continue;
      if (k === "class") {
        el.className = v as string;
      } else if (k === "style") {
        Object.assign(el.style, v as Partial<CSSStyleDeclaration>);
      } else if (k === "on") {
        for (const [evt, fn] of Object.entries(v as Record<string, EventListener>)) {
          el.addEventListener(evt, fn);
        }
      } else if (k === "dataset") {
        for (const [dk, dv] of Object.entries(v as Record<string, string>)) {
          el.dataset[dk] = dv;
        }
      } else {
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        (el as any)[k] = v;
      }
    }
  }
  for (const c of children) {
    if (c == null || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

export function clear(el: HTMLElement): void {
  while (el.firstChild) el.removeChild(el.firstChild);
}
