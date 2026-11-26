// https://dom.spec.whatwg.org/#interface-element
[Exposed=Window]
interface Element : Node {
  readonly attribute DOMString tagName;
  attribute DOMString id;
  attribute DOMString className;
  [SameObject] readonly attribute DOMTokenList classList;
  [SameObject] readonly attribute NamedNodeMap attributes;
  attribute DOMString innerHTML;
  attribute DOMString outerHTML;
  DOMString? getAttribute(DOMString qualifiedName);
  undefined setAttribute(DOMString qualifiedName, DOMString value);
  undefined removeAttribute(DOMString qualifiedName);
  boolean hasAttribute(DOMString qualifiedName);
  boolean toggleAttribute(DOMString qualifiedName, optional boolean force);
  Element? querySelector(DOMString selectors);
  NodeList querySelectorAll(DOMString selectors);
  Element? closest(DOMString selectors);
  undefined insertAdjacentHTML(DOMString position, DOMString text);
};
