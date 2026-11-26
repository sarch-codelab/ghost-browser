// https://dom.spec.whatwg.org/#interface-node
[Exposed=Window]
interface Node : EventTarget {
  const unsigned short ELEMENT_NODE = 1;
  const unsigned short ATTRIBUTE_NODE = 2;
  const unsigned short TEXT_NODE = 3;
  readonly attribute unsigned short nodeType;
  readonly attribute DOMString nodeName;
  readonly attribute DOMString? nodeValue;
  readonly attribute Node? parentNode;
  readonly attribute Element? parentElement;
  readonly attribute NodeList childNodes;
  readonly attribute Node? firstChild;
  readonly attribute Node? lastChild;
  readonly attribute Node? previousSibling;
  readonly attribute Node? nextSibling;
  Node appendChild(Node node);
  Node insertBefore(Node node, Node? child);
  Node removeChild(Node child);
  Node replaceChild(Node node, Node child);
  Node? cloneNode(optional boolean deep = false);
  boolean hasChildNodes();
  boolean contains(Node? other);
  boolean isEqualNode(Node? otherNode);
};
