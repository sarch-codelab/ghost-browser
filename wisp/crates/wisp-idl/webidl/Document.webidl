// https://dom.spec.whatwg.org/#interface-document
[Exposed=Window]
interface Document : Node {
  readonly attribute DocumentType? doctype;
  readonly attribute Element? documentElement;
  attribute DOMString title;
  readonly attribute DOMString URL;
  readonly attribute DOMString readyState;
  [SameObject] readonly attribute HTMLCollection images;
  [SameObject] readonly attribute HTMLCollection links;
  [LegacyUnforgeable] readonly attribute Location location;
  Element? getElementById(DOMString elementId);
  HTMLCollection getElementsByTagName(DOMString qualifiedName);
  HTMLCollection getElementsByClassName(DOMString classNames);
  Element? querySelector(DOMString selectors);
  NodeList querySelectorAll(DOMString selectors);
  Element createElement(DOMString localName);
  Text createTextNode(DOMString data);
  DocumentFragment createDocumentFragment();
  undefined open();
  undefined close();
  undefined write(DOMString... text);
};
