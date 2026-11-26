// https://html.spec.whatwg.org/#the-window-object
[Global=Window, Exposed=Window]
interface Window : EventTarget {
  readonly attribute WindowProxy window;
  readonly attribute WindowProxy self;
  [SameObject] readonly attribute Document document;
  readonly attribute Location location;
  readonly attribute unsigned long innerWidth;
  readonly attribute unsigned long innerHeight;
  readonly attribute boolean closed;
  attribute DOMString name;
  [Replaceable] readonly attribute any event;
  undefined alert(optional DOMString message = "");
  undefined close();
  undefined focus();
  undefined blur();
  Window? open(optional USVString url = "", optional DOMString target = "", optional DOMString windowFeatures = "");
};
