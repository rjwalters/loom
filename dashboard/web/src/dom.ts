/**
 * A ~30-line DOM builder, in lieu of a UI framework.
 *
 * Why this and not JSX/React/Svelte: see `../README.md` §"Why vanilla
 * TypeScript". The short version is that every view in this app is a pure
 * `(data) => HTMLElement` function with no local state, so a framework's
 * reconciler would have nothing to reconcile — and `el()` keeps the views
 * declarative without one.
 *
 * **Text goes in through `textContent`, never `innerHTML`.** Host ids, repo
 * names, phases, and model names all originate from remote hosts; the one
 * place this app could grow an XSS is a template-string HTML builder, so there
 * is not one.
 */

type Falsy = null | undefined | false;
export type Child = Node | string | number | Falsy | Child[];

export interface Attrs {
  class?: string;
  title?: string;
  href?: string;
  id?: string;
  type?: string;
  role?: string;
  /** Inline style, used only for data-driven geometry (a usage meter's width)
   * that cannot live in the stylesheet. */
  style?: string;
  /** `data-*` attributes, used by tests and the drill-down click target. */
  data?: Record<string, string | number | undefined>;
  /** `aria-*` attributes. */
  aria?: Record<string, string | undefined>;
  onClick?: (event: MouseEvent) => void;
}

export function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs: Attrs = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (attrs.class) node.className = attrs.class;
  if (attrs.title) node.title = attrs.title;
  if (attrs.id) node.id = attrs.id;
  if (attrs.role) node.setAttribute("role", attrs.role);
  if (attrs.style) node.setAttribute("style", attrs.style);
  if (attrs.type) node.setAttribute("type", attrs.type);
  if (attrs.href) node.setAttribute("href", attrs.href);
  for (const [key, value] of Object.entries(attrs.data ?? {})) {
    if (value !== undefined) node.setAttribute(`data-${key}`, String(value));
  }
  for (const [key, value] of Object.entries(attrs.aria ?? {})) {
    if (value !== undefined) node.setAttribute(`aria-${key}`, value);
  }
  if (attrs.onClick) node.addEventListener("click", attrs.onClick as EventListener);
  append(node, children);
  return node;
}

export function append(parent: Node, children: Child[]): void {
  for (const child of children) {
    if (child === null || child === undefined || child === false) continue;
    if (Array.isArray(child)) {
      append(parent, child);
    } else if (child instanceof Node) {
      parent.appendChild(child);
    } else {
      parent.appendChild(document.createTextNode(String(child)));
    }
  }
}

/** Replace an element's entire contents. Used once per render pass. */
export function replaceChildren(parent: Element, ...children: Child[]): void {
  parent.replaceChildren();
  append(parent, children);
}

/** A `<dt>/<dd>` pair — the fleet's metrics are all label/value, and this is
 * what keeps them a description list (screen-reader navigable) rather than a
 * pile of divs. */
export function field(label: string, value: string, title?: string): DocumentFragment {
  const fragment = document.createDocumentFragment();
  fragment.appendChild(el("dt", { class: "field__label" }, label));
  fragment.appendChild(
    el("dd", { class: "field__value", ...(title ? { title } : {}) }, value),
  );
  return fragment;
}
