// https://html.spec.whatwg.org/#htmlelement
[Exposed=Window]
interface HTMLElement : Element {
  [CEReactions] attribute DOMString title;
  [CEReactions] attribute DOMString lang;
  [CEReactions] attribute boolean hidden;
  [CEReactions] attribute DOMString dir;
  attribute DOMString innerText;
  attribute DOMString outerText;
  attribute DOMString accessKey;
  undefined click();
  undefined focus();
  undefined blur();
};
