// The DOM object model, built on the native primitives in `dom_api.rs`.
(function () {
  "use strict";

  const api = globalThis.__h5i;

  /// What a lazily parsed part of the prelude can reach.
  ///
  /// Most of this file is one closure, which is why it is cheap to write and
  /// impossible to split: every binding is in scope everywhere and nothing
  /// declares what it needs. A tier (see `TIERS` in `mod.rs`) is a *separate*
  /// `eval`, so it has no way in, and this record is the door. Sections put
  /// what a tier needs on it as they build it, and a tier takes only what it
  /// names — which makes the dependency visible in both files instead of
  /// implicit in a shared scope.
  const internals = {};
  Object.defineProperty(globalThis, "__h5iInternals", {
    value: internals, writable: false, enumerable: false, configurable: false,
  });

  /// Names that stand for a source this realm has not parsed yet.
  function lazyGlobals(tier, names) {
    const install = (name, value) => {
      Object.defineProperty(globalThis, name, {
        value, writable: true, enumerable: false, configurable: true,
      });
    };
    for (const name of names) {
      Object.defineProperty(globalThis, name, {
        configurable: true,
        enumerable: false,
        get() {
          __h5iTier(tier);
          const now = Object.getOwnPropertyDescriptor(globalThis, name);
          // A tier that loaded without defining what it was asked for would
          // otherwise return this accessor's own undefined, for ever, and look
          // exactly like an engine that never had the interface.
          if (!now || now.get) {
            throw new Error(`the ${tier} tier did not define ${name}`);
          }
          return now.value;
        },
        set(value) { install(name, value); },
      });
    }
  }

  /// Tag name to the interface that tag gets, filled in once the
  /// interfaces below exist. A Map declared here rather than beside them
  /// because `constructElement` is defined above and would otherwise read
  /// a `const` still in its temporal dead zone.
  const TAG_CLASSES = new Map();

  /// Transient user activation, the spec's gate on gesture-guarded APIs.
  ///
  /// No real user drives this engine, so the flag is armed by the paths that
  /// stand in for one — the testdriver shim's click, `h5i browser click` —
  /// and consumed by the APIs that spend it (`showPicker`). `hasBeen` never
  /// goes back down, which is exactly `navigator.userActivation`'s pair.
  const userActivation = { active: false, hasBeen: false };

  /// Which node has focus, or null for none.
  ///
  /// Held here rather than on the tree because focus is a property of the
  /// *document's* view of itself, not of a node, and two nodes must not be able
  /// to believe they both have it.
  let focusedId = null;

  /// An error with the line it came from, for the callbacks that swallow one.
  ///
  /// Every `catch` that reports and carries on — a listener, a timer, an
  /// observer — is by definition detached from the code that scheduled it, so
  /// the message is all the reader gets and a message without a location sends
  /// them looking through the whole page. These are exactly the errors that can
  /// least afford to be anonymous, and they were the ones reporting the least.
  function withStack(error) {
    return String(error) + (error && error.stack ? "\n" + error.stack : "");
  }

  // ── nodes ────────────────────────────────────────────────────────────────

  /// The three interfaces this file builds but WebIDL gives no constructor:
  /// `Attr`, `MediaList`, `MediaQueryList`. They used to reach the global
  /// through `brand()`, which threw `Illegal constructor` for a page; now that
  /// they are real classes — so their prototypes carry their members, which is
  /// the whole point — the throw has to come from the constructor instead.
  /// `internal()` is the one door in.
  let constructingInternally = false;
  function internal(build) {
    constructingInternally = true;
    try { return build(); } finally { constructingInternally = false; }
  }
  function refuseExternal(name) {
    if (!constructingInternally) {
      throw new TypeError(`Illegal constructor: ${name} is not constructible`);
    }
  }

  const wrappers = new Map(); // id -> Node, so identity holds across lookups

  /// A live-enough `NodeList`/`HTMLCollection`.
  const COLLECTION_CLASSES = {};
  {
    const declare = (name, Parent, members) => {
      const Interface = { [name]: class extends Parent {
        constructor() {
          super();
          throw new TypeError("Illegal constructor");
        }
        static get [Symbol.species]() { return Array; }
      } }[name];
      Object.defineProperty(Interface.prototype, Symbol.toStringTag, {
        value: name, configurable: true,
      });
      for (const [member, fn] of Object.entries(members ?? {})) {
        Object.defineProperty(fn, "name", { value: member });
        Object.defineProperty(Interface.prototype, member, {
          configurable: true, enumerable: true, writable: true, value: fn,
        });
      }
      // A `length` accessor on the prototype, for the interface's shape; an
      // instance's own array `length` shadows it, so this is only ever
      // reached by inspection — or by the wrong `this`, which gets the brand
      // TypeError idlharness expects.
      const lengthGetter = function () {
        if (!(this instanceof Interface) || this === Interface.prototype) {
          throw new TypeError(`Illegal invocation: length needs a ${name}`);
        }
        return Array.prototype.slice.call(this).length;
      };
      Object.defineProperty(lengthGetter, "name", { value: "get length" });
      Object.defineProperty(Interface.prototype, "length", {
        configurable: true, enumerable: true, get: lengthGetter,
      });
      COLLECTION_CLASSES[name] = Interface;
      return Interface;
    };
    const item = function (index) {
      const at = Math.trunc(Number(index)) || 0;
      return this[at] ?? null;
    };
    const namedItem = function (name) {
      const wanted = String(name);
      return this.find(
        (n) => n.id === wanted || n.getAttribute?.("name") === wanted,
      ) ?? null;
    };
    declare("NodeList", Array, { item });
    const HTMLCollection = declare("HTMLCollection", Array, { item, namedItem });
    declare("HTMLFormControlsCollection", HTMLCollection, {});
    declare("HTMLOptionsCollection", HTMLCollection, {});
    declare("FileList", Array, { item });
    // The two CSSOM lists. Both were real arrays and real objects already —
    // `document.styleSheets[0]` and `sheet.cssRules[0]` have always worked —
    // and both were *untyped*, so `sheet.cssRules instanceof CSSRuleList` was a
    // ReferenceError over an object that was exactly that. Same shape as the
    // node lists above, and the same fix.
    declare("CSSRuleList", Array, { item });
    declare("StyleSheetList", Array, { item });
  }

  function collection(nodes, label) {
    const list = nodes.slice();
    const Interface = COLLECTION_CLASSES[label] ?? COLLECTION_CLASSES.NodeList;
    Object.setPrototypeOf(list, Interface.prototype);
    return list;
  }

  /// Take a node out of whatever parent it is in, before putting it somewhere.
  function detachFromParent(node) {
    if (!node || node._id === undefined || node._id === null) return;
    if (api.parent(node._id) === null || api.parent(node._id) === undefined) return;
    api.removeNode(node._id);
  }

  /// The id of the document node, worked out once. It is the parent of the
  /// root element and never changes.
  let knownDocumentNode;
  function documentNodeId() {
    if (knownDocumentNode === undefined) knownDocumentNode = api.parent(api.root());
    return knownDocumentNode;
  }

  /// Tells "wrap the node that already has this id" apart from "a page called `new
  /// Text('hello')`".
  const FROM_ID = Symbol("h5i node id");

  function wrap(id) {
    if (id === null || id === undefined) return null;
    let existing = wrappers.get(id);
    if (existing) return existing;
    // Re-entrant construction — a custom element's constructor asking the
    // document for itself — gets a plain wrapper rather than recursing forever.
    if (constructing.has(id)) {
      const partial = new Element(id);
      partial._kind = 1;
      return partial;
    }


    // The tree decides what a node is. A set of ids on this side only knew
    // about comments script had made, so every comment the *parser* produced
    // was wrapped as a text node.
    let raw;
    let label;
    const kind = api.nodeKind(id);
    if (kind === 8) { raw = new Comment(id, FROM_ID); label = "Comment"; }
    else if (kind === 1) { raw = constructElement(id); label = "Element"; }
    else { raw = new Text(id, FROM_ID); label = "Text"; }

    // Labelled by what the node actually is. Calling a text node "Element"
    // reported `Element.tagName` as missing when what happened was a page
    // reading `tagName` off a text node, where no engine has one.
    // Read back by the sentinel at the end of every node's prototype chain,
    // which is where a missing property is named. `label` above is what it
    // says; this is how it reaches the trap without a wrapper per node.
    raw._kind = kind;
    wrappers.set(id, raw);
    return raw;
  }

  // ── custom elements ──────────────────────────────────────────────────────
  //
  // The corpus asked for `customElements.define` once it could get that far,
  // which happened only after `HTMLElement` existed for `class X extends
  // HTMLElement` to name. Defining without upgrading would be the worse kind of
  // half-support: the page would register its components, see no error, and
  // render nothing.

  const definitions = new Map();
  const constructing = new Set();
  // Which custom elements have had `connectedCallback` run. Kept beside the
  // nodes rather than on them: a flag stored as a property is a property, and
  // the reporting proxy rightly named our own bookkeeping as a missing API the
  // first time a page's code reached a node before we had set it.
  const connected = new Set();
  const comments = new Set();
  let upgrading = null;
  let customizedCount = 0;

  /// The definition governing an element, which the tag name alone cannot find.
  ///
  /// A **customized built-in** is an ordinary tag whose `is` attribute names
  /// the definition — `<a is="fancy-link">` is an `HTMLAnchorElement` and a
  /// `FancyLink` at once — so `is` is consulted first, and only when its
  /// definition really extends this tag. An autonomous definition is matched by
  /// tag, and one that extends something is *not*, or `<fancy-link>` written as
  /// a bare tag would upgrade against a definition that never claimed it.
  function definitionFor(id) {
    const tag = api.tagName(id).toLowerCase();
    // `customizedCount` guards a **host call on the element-construction hot
    // path**. Reading `is` off every element built would be a real per-element
    // cost paid by every page, and almost no page defines a customized
    // built-in — so the read happens only once one exists.
    if (customizedCount) {
      const is = api.getAttr(id, "is");
      const customized = is && definitions.get(String(is).toLowerCase());
      if (customized && customized.extendsTag === tag) return customized;
    }
    const autonomous = definitions.get(tag);
    return autonomous && !autonomous.extendsTag ? autonomous : undefined;
  }

  function constructElement(id) {
    const tag = api.tagName(id).toLowerCase();
    const definition = definitionFor(id);
    if (!definition) return new (TAG_CLASSES.get(tag) ?? Element)(id);

    const previousUpgrade = upgrading;
    upgrading = id;
    constructing.add(id);
    try {
      return new definition.ctor();
    } catch (error) {
      // A component whose constructor throws must not take the page with it.
      console.error(`custom element <${definition.name}> threw while upgrading: ${error}`);
      return new Element(id);
    } finally {
      upgrading = previousUpgrade;
      constructing.delete(id);
    }
  }

  function isCustom(node) {
    return !!node && node.nodeType === 1 && definitionFor(node._id) !== undefined;
  }

  /// Every custom element at or under `node`, in document order.
  function collectCustom(node) {
    const found = [];
    if (definitions.size === 0) return found;
    const visit = (n) => {
      if (!n || n.nodeType !== 1) return;
      if (isCustom(n)) found.push(n);
      for (const kid of n.children) visit(kid);
    };
    visit(node);
    return found;
  }

  function notifyConnection(node) {
    // Nothing to notify if nothing is defined, and most pages define nothing.
    // Without this every insertion walked to the root and then over the whole
    // inserted subtree to find custom elements that could not exist — which
    // made attaching a node cost three times what building one detached does.
    if (definitions.size === 0) return;
    if (!node || node.nodeType !== 1 || !node.isConnected) return;
    for (const found of collectCustom(node)) fireConnected(found);
  }

  /// HTML's JavaScript MIME type essence list — the exact sixteen. Note what
  /// is missing: `javascript1.6` and `1.7` never made the standard, and the
  /// WPT type/language files assert both directions of that history.
  const JS_MIME_TYPES = new Set([
    "application/ecmascript", "application/javascript",
    "application/x-ecmascript", "application/x-javascript",
    "text/ecmascript", "text/javascript", "text/javascript1.0",
    "text/javascript1.1", "text/javascript1.2", "text/javascript1.3",
    "text/javascript1.4", "text/javascript1.5", "text/jscript",
    "text/livescript", "text/x-ecmascript", "text/x-javascript",
  ]);

  /// Prepare-a-script's type decision: "classic", "module", or null for a
  /// block that is data, not code. The legacy `language` attribute is the
  /// spec's rule, not a guess: absent-or-empty type with a non-empty language
  /// means `text/<language>`, which then faces the same sixteen-entry list.
  function scriptKindOf(el) {
    const typeAttr = api.getAttr(el._id, "type");
    const langAttr = api.getAttr(el._id, "language");
    let type;
    if (typeAttr !== null && typeAttr.trim() !== "") type = typeAttr.trim();
    else if (typeAttr === null && langAttr !== null && langAttr !== "") {
      type = `text/${langAttr}`;
    } else type = "text/javascript";
    const lower = type.toLowerCase();
    if (lower === "module") return "module";
    return JS_MIME_TYPES.has(lower) ? "classic" : null;
  }

  /// Script-inserted scripts run — synchronously for inline code, as a fetch
  /// for external, never for `innerHTML` (which does not come through here,
  /// exactly as the spec has it). This is the other half of `run_scripts` on
  /// the Rust side, which executes what the *parser* saw: a `<script>` a page
  /// builds and appends afterwards was collected by nobody, so a loader that
  /// works by injecting script tags did nothing at all.
  function runInsertedScripts(root) {
    if (!root || root.nodeType !== 1 || !root.isConnected) return;
    const found = root.tagName === "SCRIPT" ? [root] : [];
    if (typeof root.querySelectorAll === "function") {
      found.push(...root.querySelectorAll("script"));
    }
    for (const el of found) prepareInsertedScript(el);
  }

  function prepareInsertedScript(el) {
    if (el.__h5iScriptStarted || !el.isConnected) return;
    const kind = scriptKindOf(el);
    if (kind === null) return;
    const src = api.getAttr(el._id, "src");
    if (src !== null) {
      el.__h5iScriptStarted = true;
      if (src === "") {
        // An empty src is an error the element reports, not a fetch.
        queueMicrotask(() => el.dispatchEvent(new Event("error")));
        return;
      }
      // Fetched and then run in global scope, with the load/error event the
      // element owes its page. The ordering guarantees of parser-time
      // scripts (async/defer) do not apply to a script-inserted external
      // script anyway: it runs when it arrives.
      fetch(el._resolved("src"))
        .then((response) => {
          if (!response.ok) throw new Error(`HTTP ${response.status}`);
          return response.text();
        })
        .then((code) => {
          if (kind === "classic") (0, eval)(code);
          el.dispatchEvent(new Event("load"));
        })
        .catch(() => el.dispatchEvent(new Event("error")));
      return;
    }
    const code = el.textContent;
    if (!code || !code.trim()) return;
    if (kind !== "classic") return;
    el.__h5iScriptStarted = true;
    try {
      (0, eval)(code);
    } catch (error) {
      console.error(`inserted script threw: ${withStack(error)}`);
    }
  }

  function fireConnected(node) {
    if (connected.has(node._id)) return;
    connected.add(node._id);
    try {
      if (typeof node.connectedCallback === "function") node.connectedCallback();
    } catch (error) {
      console.error(`custom element connectedCallback threw: ${withStack(error)}`);
    }
  }

  function fireDisconnected(node) {
    if (!connected.has(node._id)) return;
    connected.delete(node._id);
    try {
      if (typeof node.disconnectedCallback === "function") node.disconnectedCallback();
    } catch (error) {
      console.error(`custom element disconnectedCallback threw: ${withStack(error)}`);
    }
  }

  function fireAttributeChanged(node, name, oldValue, newValue) {
    if (!isCustom(node)) return;
    const observedNames = node.constructor && node.constructor.observedAttributes;
    if (!Array.isArray(observedNames) || !observedNames.includes(name)) return;
    try {
      if (typeof node.attributeChangedCallback === "function") {
        // Four arguments: the namespace rides along, and it is `null` — not
        // absent — for the attributes this engine writes. WPT logs all four
        // and compares each against null.
        node.attributeChangedCallback(name, oldValue, newValue, null);
      }
    } catch (error) {
      console.error(`custom element attributeChangedCallback threw: ${withStack(error)}`);
    }
  }

  const pendingDefinitions = new Map();

  /// Whether a string is a valid custom element name, per HTML §4.13.
  const NAME_START = /[:A-Z_a-z\u00C0-\u00D6\u00D8-\u00F6\u00F8-\u02FF\u0370-\u037D\u037F-\u1FFF\u200C-\u200D\u2070-\u218F\u2C00-\u2FEF\u3001-\uD7FF\uF900-\uFDCF\uFDF0-\uFFFD]/;
  const NAME_CHAR = /[-.0-9\u00B7\u0300-\u036F\u203F-\u2040:A-Z_a-z\u00C0-\u00D6\u00D8-\u00F6\u00F8-\u02FF\u0370-\u037D\u037F-\u1FFF\u200C-\u200D\u2070-\u218F\u2C00-\u2FEF\u3001-\uD7FF\uF900-\uFDCF\uFDF0-\uFFFD]/;

  function validateQualifiedName(name) {
    const bad = (why) => {
      throw new DOMException(why, "InvalidCharacterError");
    };
    if (name.length === 0) bad("the name must not be empty");
    if (!NAME_START.test(name[0])) {
      bad(`\`${name}\` does not start with a name character`);
    }
    for (const character of name.slice(1)) {
      if (!NAME_CHAR.test(character)) {
        bad(`\`${name}\` contains \`${character}\`, which is not a name character`);
      }
    }
    // A qualified name is `prefix:local`, so at most one colon, and neither
    // half may be empty.
    const parts = name.split(":");
    if (parts.length > 2) bad(`\`${name}\` has more than one colon`);
    if (parts.length === 2 && (parts[0] === "" || parts[1] === "")) {
      bad(`\`${name}\` has an empty prefix or local name`);
    }
    if (parts.length === 2 && !NAME_START.test(parts[1][0])) {
      bad(`the local name in \`${name}\` does not start with a name character`);
    }
  }

  const RESERVED_ELEMENT_NAMES = new Set([
    "annotation-xml", "color-profile", "font-face", "font-face-src",
    "font-face-uri", "font-face-format", "font-face-name", "missing-glyph",
  ]);

  /// The characters a name may hold, after the first.
  const FORBIDDEN_IN_LOCAL_NAME = new Set([
    0x00, 0x09, 0x0a, 0x0c, 0x0d, 0x20, 0x2f, 0x3e,
  ]);

  function isValidCustomElementName(name) {
    if (typeof name !== "string" || name.length === 0) return false;
    if (!name.includes("-")) return false;
    if (RESERVED_ELEMENT_NAMES.has(name)) return false;
    // Must begin with an ASCII lower alpha, which also settles which branch of
    // "valid element local name" applies: the one that starts with a letter.
    const first = name.codePointAt(0);
    if (first < 0x61 || first > 0x7a) return false;
    // Iterated by code *point*, so an astral character counts once and its
    // surrogate halves are never tested on their own.
    for (const character of name) {
      const code = character.codePointAt(0);
      if (code >= 0x41 && code <= 0x5a) return false;
      if (FORBIDDEN_IN_LOCAL_NAME.has(code)) return false;
    }
    return true;
  }

  const customElements = {
    define(name, ctor, options) {
      // **The name is validated, not merely checked for a dash.** The dash rule
      // is one clause of eight, and the others are not decoration: a name that
      // is uppercase, or that starts with a digit, or that collides with one of
      // the eight reserved SVG/MathML compound names, is a name a browser
      // refuses and this engine used to accept — after which the page and the
      // engine disagree about what `<font-face>` is.
      const key = String(name);
      if (!isValidCustomElementName(key)) {
        throw new DOMException(
          `\`${key}\` is not a valid custom element name`,
          "SyntaxError",
        );
      }
      if (definitions.has(key)) {
        throw new Error(`custom element \`${key}\` is already defined`);
      }
      /// `{ extends: "a" }` — a **customized built-in**, which is a real
      /// definition here rather than a refusal.
      ///
      /// The one case the spec singles out is extending a name that is itself a
      /// valid custom element name: two definitions would then race for one
      /// tag, and neither `is` nor the tag could say which won.
      let extendsTag = null;
      if (options && options.extends != null) {
        extendsTag = String(options.extends).toLowerCase();
        if (isValidCustomElementName(extendsTag)) {
          throw new DOMException(
            `\`${extendsTag}\` is a custom element name and cannot be extended`,
            "NotSupportedError",
          );
        }
      }
      definitions.set(key, { name: key, ctor, extendsTag });
      if (extendsTag) customizedCount++;

      // Upgrade what is already on the page. Without this, a page that ships
      // its markup server-side and defines its components afterwards — which is
      // most of them — would define and then do nothing.
      // A customized built-in is not found by its own name: it is on the page
      // as the tag it extends, wearing `is`.
      for (const id of api.queryAll(extendsTag ? `${extendsTag}[is="${key}"]` : key, 0)) {
        wrappers.delete(id);
        const node = wrap(id);
        // The observed attributes have their initial values delivered, as they
        // are on upgrade in a real engine, or a component that only renders
        // from `attributeChangedCallback` renders blank.
        const observedNames = ctor.observedAttributes;
        if (Array.isArray(observedNames)) {
          for (const attribute of observedNames) {
            const value = api.getAttr(id, attribute);
            if (value !== null) fireAttributeChanged(node, attribute, null, value);
          }
        }
        if (node.isConnected) fireConnected(node);
      }

      const waiting = pendingDefinitions.get(key);
      if (waiting) { pendingDefinitions.delete(key); waiting.forEach((resolve) => resolve(ctor)); }
    },
    get(name) {
      const definition = definitions.get(String(name).toLowerCase());
      return definition ? definition.ctor : undefined;
    },
    getName(ctor) {
      for (const [key, definition] of definitions) {
        if (definition.ctor === ctor) return key;
      }
      return null;
    },
    whenDefined(name) {
      const key = String(name);
      // **An invalid name rejects rather than waiting forever.** `define` and `whenDefined`
      // validate the same way, and a promise that never settles is the worst of the three
      // possible answers: a caller cannot tell it from a component that has not loaded yet, so
      // it waits out its own timeout instead of handling an error it could have handled at
      // once.
      if (!isValidCustomElementName(key)) {
        return Promise.reject(new DOMException(
          `\`${key}\` is not a valid custom element name`,
          "SyntaxError",
        ));
      }
      const definition = definitions.get(key);
      if (definition) return Promise.resolve(definition.ctor);
      return new Promise((resolve) => {
        const waiting = pendingDefinitions.get(key) || [];
        waiting.push(resolve);
        pendingDefinitions.set(key, waiting);
      });
    },
    upgrade(node) {
      for (const found of collectCustom(node)) {
        if (found.isConnected) fireConnected(found);
      }
    },
  };

  /// Every node in the tree, in document order. Used for the two questions that
  /// need it — `compareDocumentPosition` and the traversal objects — and
  /// rebuilt each time, because script mutates the tree between calls.
  function documentOrder() {
    const out = [];
    const visit = (id) => {
      out.push(id);
      for (const kid of api.children(id)) visit(kid);
    };
    visit(api.root());
    return out;
  }

  // Report a read that found nothing, without taxing the reads that find something.

  /// The labels `wrap` gives the three kinds of node it hands out.
  const KIND_LABELS = { 1: "Element", 3: "Text", 8: "Comment" };

  /// One stand-in per label, because building a fresh proxy per object was the
  /// other half of the old cost: `el.classList` minted one on every read.
  const sentinels = new Map();

  /// Objects that *are* sentinels, so a second `observed` on the same target
  /// stacks nothing.
  const isSentinel = new Set();

  function gapTrap(naming) {
    return {
      get(object, property, receiver) {
        // Found after all: the sentinel stands in for the tail of a real
        // prototype chain, and `Object.prototype`'s members are still there.
        if (typeof property === "symbol" || property in object) {
          return Reflect.get(object, property, receiver);
        }
        // `then` is probed by the promise machinery on anything it is handed;
        // recording it would report a missing API every time a node passed
        // through an await.
        if (property === "then") return undefined;

        // Nor is a page's own bookkeeping. No web platform property begins with
        // an underscore or a dollar; frameworks' private fields routinely do.
        // Solid reads `document._$DX_DELEGATE` before it sets it, and the list
        // an agent reads carried that as something this engine was missing.
        //
        // First, and not only for tidiness: `naming` below reads `_kind` off
        // the receiver, and a receiver without one would come back here for
        // that read. This rule is what makes that terminate.
        const name = String(property);
        const first = name.charCodeAt(0);
        if (first === 95 || first === 36) {
          // Ours, and therefore a cost rather than a gap: see
          // `declareInternals`. Counted only when a test asks, because the
          // answer to "did one of our own reads walk the whole chain" is
          // otherwise invisible — it is undefined either way.
          if (globalThis.__h5iReportInternalMisses) api.unsupported(`internal miss: ${name}`);
          return undefined;
        }

        // Nor is a generated key. jQuery and Sizzle stamp elements with names
        // like `jQuery360062973586668224961` and `sizzle1786301869537`, read
        // before they are written — one page produced 5265 such "gaps" and put
        // them at the top of the list, which is exactly the burying this filter
        // exists to prevent. No web platform property carries a run of digits
        // that long, because it would have to be typed by a person.
        if (/\d{6}/.test(name)) return undefined;

        // Something we never handed out. A page's own `class Store extends
        // EventTarget` reaches no sentinel, but a bare `new Element(id)` this
        // file made for its own use does, and that is not an object the page
        // asked us for.
        const label = naming(receiver);
        if (label === null || label === undefined) return undefined;

        // A gap is only a gap if a real browser would have answered. Reading
        // `tagName` off a text node gets undefined in every engine there is —
        // reporting it would send us building something that does not exist,
        // and the corpus did exactly that until this rule was written.
        if (label !== "Element" && property in Element.prototype) return undefined;

        api.unsupported(`${label}.${name}`);
        return undefined;
      },
    };
  }

  /// The stand-in that goes where `proto` was.
  ///
  /// Its target is an empty object *over* `proto` rather than `proto` itself,
  /// which is what keeps `instanceof` honest: a proxy reports its target's
  /// prototype, so the real chain continues through it instead of ending at it.
  function gapSentinel(proto, label) {
    const naming = typeof label === "function" ? label : () => label;
    const found = sentinels.get(label);
    if (found !== undefined && Object.getPrototypeOf(found) === proto) return found;
    const sentinel = new Proxy(Object.create(proto), gapTrap(naming));
    isSentinel.add(sentinel);
    if (found === undefined) sentinels.set(label, sentinel);
    return sentinel;
  }

  /// Watch one object: for the singletons this file builds as literals, where
  /// every property it really has is an own property and the sentinel is
  /// reached only by a read that missed.
  function observed(target, label) {
    const proto = Object.getPrototypeOf(target);
    if (isSentinel.has(proto)) return target;
    Object.setPrototypeOf(target, gapSentinel(proto, label));
    return target;
  }

  /// Declare the fields this file sets only sometimes.
  function declareInternals(proto, names) {
    for (const name of names) {
      Object.defineProperty(proto, name, {
        value: undefined, writable: true, enumerable: false, configurable: true,
      });
    }
  }

  /// Watch every instance of a class, once, by putting the sentinel at the end
  /// of the class's own chain. The per-instance form would sit *above* the
  /// instance and below the prototype holding its methods, which puts a trap
  /// back in front of every method call — the cost this rewrite removes.
  function observedClass(Interface, label) {
    const proto = Object.getPrototypeOf(Interface.prototype);
    if (isSentinel.has(proto)) return;
    Object.setPrototypeOf(Interface.prototype, gapSentinel(proto, label));
  }

  // `class` is the famous one, but `rel` is a token list too, and so are
  // `sandbox` and `headers`. Parameterising the attribute is the difference
  // between one implementation and four.
  /// Tokenised class strings, keyed on the attribute text they came from.
  /// See `DOMTokenList._all`.
  const tokenSets = new Map();

  /// `/[ \t\n\f\r]/.test(name)` without the regex, for the same reason.
  function hasAsciiWhitespace(text) {
    for (let at = 0; at < text.length; at++) {
      const code = text.charCodeAt(at);
      if (code === 32 || code === 9 || code === 10 || code === 12 || code === 13) {
        return true;
      }
    }
    return false;
  }

  class DOMTokenList {
    constructor(node, attribute) { this._node = node; this._attr = attribute; }
    /// The ordered token *set*: `class="a a b"` is two tokens, not three, and `length`,
    /// iteration and indexing all see the deduplicated view.
    _all() {
      const raw = api.getAttr(this._node._id, this._attr);
      if (!raw) return [];
      const known = tokenSets.get(raw);
      if (known !== undefined) return known;
      const out = [];
      let start = -1;
      for (let at = 0; at <= raw.length; at++) {
        const code = at < raw.length ? raw.charCodeAt(at) : 32;
        const space = code === 32 || code === 9 || code === 10
          || code === 12 || code === 13;
        if (!space) {
          if (start < 0) start = at;
          continue;
        }
        if (start >= 0) {
          const token = raw.slice(start, at);
          if (!out.includes(token)) out.push(token);
          start = -1;
        }
      }
      if (tokenSets.size > 512) tokenSets.clear();
      tokenSets.set(raw, out);
      return out;
    }
    /// Through `setAttribute`, **not** `api.setAttr`.
    ///
    /// The raw host call skips everything `Element.setAttribute` does on the
    /// way past: the mutation record and the custom element's
    /// `attributeChangedCallback`. So `classList.add(...)` changed the class
    /// and no MutationObserver saw it — which is a live defect for any
    /// framework watching attributes, and it is what `Element-classlist`
    /// detects by asking for "a mutation exactly when replace() returns true".
    _write(list) {
      this._node.setAttribute(this._attr, list.join(" "));
    }
    /// The spec's "update steps", and **which callers run them is the whole contract** — a
    /// first attempt made the write conditional on the set having changed, which is wrong for
    /// three of the four mutators.
    _update(list) {
      if (list.length === 0 && api.getAttr(this._node._id, this._attr) === null) return;
      this._write(list);
    }
    /// Every mutating method validates first, and all of them the same way.
    ///
    /// An empty token and a token containing whitespace are both errors with
    /// names the spec gives them, and they are not pedantry: `classList.add("")`
    /// silently wrote a trailing space, and `classList.add("a b")` wrote a
    /// token that then read back as *two* — so a page that added one class
    /// could not remove it again.
    _check(names) {
      // **Both passes, in this order.** The spec throws SyntaxError if *any*
      // token is empty before it looks at whitespace in any of them, so
      // `replace(" ", "")` is a SyntaxError for the empty second argument, not
      // an InvalidCharacterError for the first.
      const tokens = names.map(String);
      if (tokens.includes("")) {
        throw new DOMException("the token must not be empty", "SyntaxError");
      }
      const spaced = tokens.find(hasAsciiWhitespace);
      if (spaced !== undefined) {
        throw new DOMException(
          `the token \`${spaced}\` must not contain whitespace`,
          "InvalidCharacterError",
        );
      }
      return tokens;
    }
    add(...names) {
      const wanted = this._check(names);
      const list = this._all();
      const next = [...list];
      for (const n of wanted) if (!next.includes(n)) next.push(n);
      this._update(next);
    }
    remove(...names) {
      const unwanted = this._check(names);
      this._update(this._all().filter((n) => !unwanted.includes(n)));
    }
    /// Swap one token for another, keeping its position.
    ///
    /// Absent, and asked for 262 times across the corpus: it is how a component
    /// moves between states without a remove-then-add that briefly has neither
    /// class, which matters when a stylesheet transitions on the change.
    replace(oldToken, newToken) {
      const [from, to] = this._check([oldToken, newToken]);
      const list = this._all();
      if (!list.includes(from)) return false;
      // Swap in place, then drop duplicates — which is the spec's own order and
      // handles the two cases a special case got wrong: replacing a token with
      // *itself* left the list alone (it used to splice the token out), and
      // replacing it with one already present keeps the earlier position.
      this._update([...new Set(list.map((token) => (token === from ? to : token)))]);
      return true;
    }
    // `String(name)`, because `classList.contains(null)` asks about the token
    // "null" — DOMString conversion happens before the lookup, and comparing
    // the raw value against a list of strings answered false for every
    // non-string a page passed.
    contains(name) { return this._all().includes(String(name)); }
    item(index) { return this._all()[index] ?? null; }
    // False, and deliberately. `supports` asks whether *this engine* acts on a
    // token — `rel="preload"`, `sandbox="allow-scripts"` — and this one acts on
    // none of them. Answering true would send a page down a path expecting
    // behaviour that will not happen, which is the plausible-wrong answer this
    // engine keeps having to refuse.
    supports(token) {
      // `class` defines no supported tokens at all, and the spec's answer to
      // asking is a TypeError, not a polite false.
      if (this._attr === "class") {
        throw new TypeError("classList.supports: class has no supported tokens");
      }
      void token;
      return false;
    }
    get length() { return this._all().length; }
    get value() { return api.getAttr(this._node._id, this._attr) || ""; }
    set value(v) { this._node.setAttribute(this._attr, String(v)); }
    forEach(fn, thisArg) { this._all().forEach(fn, thisArg); }
    keys() { return this._all().keys(); }
    values() { return this._all().values(); }
    entries() { return this._all().entries(); }
    [Symbol.iterator]() { return this._all()[Symbol.iterator](); }
    /// `list[0]`, which is indexed access and not a property.
    ///
    /// A `DOMTokenList` is an indexed collection, so `classList[0]` is its
    /// first token and `classList[-1]` is `undefined`. Both answered
    /// `undefined` here, and the second is right for the wrong reason — the
    /// reporting picked it up as `DOMTokenList.0` and `DOMTokenList.-1`, an
    /// engine gap recorded under two names because nothing implemented either.
    static _indexed(target) {
      // Methods and getters run against the **target**, never the proxy.
      //
      // `this` inside `add` reads `_node` and `_attr`, and when `this` is the
      // proxy every one of those internal reads pays a trap — the same cost
      // the note above `collection()` records for wrapping a NodeList, and the
      // reason `contains` measured 17 us after its tokenising had already been
      // made free. Binding once and caching keeps method identity stable, which
      // pages compare.
      const bound = new Map();
      return new Proxy(target, {
        get(list, key, receiver) {
          void receiver;
          if (typeof key === "string" && /^(0|[1-9][0-9]*)$/.test(key)) {
            return list._all()[Number(key)];
          }
          const value = Reflect.get(list, key, list);
          if (typeof value !== "function") return value;
          let fn = bound.get(key);
          if (fn === undefined) { fn = value.bind(list); bound.set(key, fn); }
          return fn;
        },
        has(list, key) {
          if (typeof key === "string" && /^(0|[1-9][0-9]*)$/.test(key)) {
            return Number(key) < list._all().length;
          }
          return Reflect.has(list, key);
        },
      });
    }
    // The stringifier is `value` — the attribute's *raw* text, not the
    // deduplicated join. A second `toString` further down returned the join and
    // won by being later; it is gone, and this is the spec's answer.
    toString() { return this.value; }
    get [Symbol.toStringTag]() { return "DOMTokenList"; }
    /// The one mutator that can decline to update: `toggle(token, true)` on a
    /// list that already has the token, and `toggle(token, false)` on one that
    /// does not, both answer without touching the attribute.
    toggle(name, force) {
      // `_check` first, and on **every** path: the spec validates the token
      // before it looks at `force`, so `toggle("", false)` is a SyntaxError
      // rather than a quiet `false`. The declining paths below never reach
      // `add`/`remove`, which is where validation used to happen.
      const [token] = this._check([name]);
      // `force` is an *optional boolean*, so WebIDL converts it — any truthy
      // value means true. Comparing `=== true` made `toggle(cls, list.length)`
      // remove the class it was asked to keep.
      const forced = force === undefined ? undefined : !!force;
      if (this.contains(token)) {
        if (forced === true) return true;
        this.remove(token);
        return false;
      }
      if (forced === false) return false;
      this.add(token);
      return true;
    }
  }

  /// The base every event-dispatching thing extends, including code that has
  /// nothing to do with the document.
  ///
  /// Frameworks write `class Store extends EventTarget`, and its absence was a
  /// bare `ReferenceError: EventTarget is not defined` that took down whole
  /// bundles. Deliberately *not* the DOM's `Node`: a store is not in the tree,
  /// and giving it a node id it does not have would be the plausible-wrong
  /// answer this engine keeps having to avoid.
  class EventTarget {
    addEventListener(type, handler, options) {
      if (typeof handler !== "function" && typeof handler?.handleEvent !== "function") return;
      (this.__listeners ??= new Map()).set(handler, { type: String(type), options });
    }
    removeEventListener(type, handler) {
      void type;
      this.__listeners?.delete(handler);
    }
    dispatchEvent(event) {
      if (!event || typeof event.type !== "string") return true;
      if (event.target === null || event.target === undefined) {
        try { event.target = this; event.currentTarget = this; } catch (_) {}
      }
      for (const [handler, registered] of this.__listeners ?? []) {
        if (registered.type !== event.type) continue;
        try {
          if (typeof handler === "function") handler.call(this, event);
          else handler.handleEvent(event);
        } catch (error) {
          console.error(`a ${event.type} listener threw: ${withStack(error)}`);
        }
        if (registered.options && registered.options.once) this.__listeners.delete(handler);
      }
      return !event.defaultPrevented;
    }
  }

  class Node {
    constructor(id) {
      // `undefined` means "the element currently being upgraded".
      if (typeof id !== "number" && upgrading === null) {
        // `new MyElement()` on a **defined** element is legal and creates one:
        // HTML says the constructor makes an element with the definition's own
        // local name — the extended tag plus `is` for a customized built-in.
        // Only an interface with no definition behind it is "not
        // constructible", which is what the message below now means.
        const named = new.target && customElements.getName(new.target);
        const def = named ? definitions.get(named) : undefined;
        if (def) {
          const made = api.createElement(def.extendsTag ?? def.name);
          if (def.extendsTag) api.setAttr(made, "is", def.name);
          this._id = made;
          wrappers.set(made, this);
          return;
        }
        throw new TypeError(
          "Illegal constructor: this interface is not constructible",
        );
      }
      this._id = id === undefined ? upgrading : id;
    }

    get ownerDocument() { return document; }
    get isConnected() { return api.isConnected(this._id); }
    getRootNode() {
      // The document when attached, the top of the detached fragment when not
      // — which is how code decides whether it is inside the page yet.
      if (this.isConnected) return document;
      let top = this;
      while (top.parentNode) top = top.parentNode;
      return top;
    }
    // `contains` and `compareDocumentPosition` were each defined **twice** in
    // this class, and the later definition won — so the pair that used to stand
    // here was dead code the whole time. The surviving
    // `compareDocumentPosition` is the spec's full bit field rather than this
    // one's approximation, which is why nothing noticed.

    // Cached at wrap time. A node's kind is fixed when it is created, and this
    // is read constantly — every `nodeType === 1` filter, every tree walk, and
    // `children` on top of that — so paying a call into the tree for a constant
    // was 1.9 µs on the hottest property in the DOM.
    get nodeType() {
      return this._kind !== undefined ? this._kind : api.nodeKind(this._id);
    }
    get parentNode() {
      const parent = api.parent(this._id);
      if (parent === null || parent === undefined) return null;
      // The parent of `<html>` is the document, and it has to *be* the document
      // — code walks up until it finds node type 9 and then asks that thing for
      // `body` and `documentElement`. Compared against a remembered id rather
      // than asked of the tree, because this is a walk: one call per ancestor
      // was two.
      if (parent === documentNodeId()) return document;
      return wrap(parent);
    }
    get parentElement() {
      const parent = this.parentNode;
      return parent && parent.nodeType === 1 ? parent : null;
    }
    get childNodes() {
      // A `<template>`'s children belong to its `content` fragment, not to the
      // element: `tp.childNodes.length` is 0 in a browser however much markup
      // it holds. This engine keeps one node for both, so the element hides
      // them and `TemplateContent` shows them — which is the division the spec
      // describes anyway. Without it a walker that recurses into a template
      // reads markup the page has not rendered and may never render.
      if (this.tagName === "TEMPLATE") return [];
      return api.children(this._id).map(wrap);
    }
    /// The single most-asked-for thing this engine did not have.
    ///
    /// WPT called it 3,944 times across the corpus, more than twice anything
    /// else on the list, and it is one line. It went missing because nothing in
    /// four hand-picked corpora used it and everything in the DOM test suite
    /// does — which is the argument for running a conformance suite in one
    /// sentence.
    hasChildNodes() { return api.children(this._id).length > 0; }

    /// Same type, same name, same attributes, same children — not the same node.
    isEqualNode(other) {
      if (!other) return false;
      if (this.nodeType !== other.nodeType) return false;
      if (this.nodeType === 3 || this.nodeType === 8) return this.data === other.data;
      if (this.tagName !== other.tagName) return false;
      const mine = api.attrNames(this._id) || [];
      const theirs = api.attrNames(other._id) || [];
      if (mine.length !== theirs.length) return false;
      for (const name of mine) {
        if (api.getAttr(this._id, name) !== api.getAttr(other._id, name)) return false;
      }
      const a = this.childNodes, b = other.childNodes;
      if (a.length !== b.length) return false;
      for (let i = 0; i < a.length; i++) if (!a[i].isEqualNode(b[i])) return false;
      return true;
    }
    isSameNode(other) { return !!other && other._id === this._id; }

    /// Merge adjacent text nodes and drop empty ones.
    normalize() {
      const kids = this.childNodes;
      let previous = null;
      for (const kid of kids) {
        if (kid.nodeType === 3) {
          if (kid.data === "") { kid.remove(); continue; }
          if (previous) { previous.data += kid.data; kid.remove(); continue; }
          previous = kid;
        } else {
          previous = null;
          if (kid.nodeType === 1) kid.normalize();
        }
      }
    }

    /// Where `other` sits relative to this node, as the spec's bit field.
    compareDocumentPosition(other) {
      if (!other) return 1;
      if (other._id === this._id) return 0;
      const DISCONNECTED = 1, PRECEDING = 2, FOLLOWING = 4, CONTAINS = 8, CONTAINED = 16;
      const ancestors = (node) => { const out = []; for (let n = node; n; n = n.parentNode) out.push(n); return out; };
      const mine = ancestors(this), theirs = ancestors(other);
      if (theirs.some((n) => n._id === this._id)) return FOLLOWING | CONTAINED;
      if (mine.some((n) => n._id === other._id)) return PRECEDING | CONTAINS;
      // Nearest common ancestor, then compare the branches under it.
      const common = mine.find((a) => theirs.some((b) => b._id === a._id));
      if (!common) return DISCONNECTED | PRECEDING;
      const branchOf = (chain) => chain[chain.findIndex((n) => n._id === common._id) - 1];
      const a = branchOf(mine), b = branchOf(theirs);
      const kids = common.childNodes;
      let seenA = -1, seenB = -1;
      for (let i = 0; i < kids.length; i++) {
        if (a && kids[i]._id === a._id) seenA = i;
        if (b && kids[i]._id === b._id) seenB = i;
      }
      return seenA < seenB ? FOLLOWING : PRECEDING;
    }
    get firstChild() { return this.childNodes[0] || null; }
    get lastChild() { const c = this.childNodes; return c[c.length - 1] || null; }

    // Text for a text node, null for an element — the distinction is the whole
    // reason the property exists, and code that walks a tree branches on it.
    get nodeName() {
      if (this.nodeType === 3) return "#text";
      if (this.nodeType === 8) return "#comment";
      if (this.nodeType === 11) return "#document-fragment";
      return api.tagName(this._id);
    }

    get nodeValue() { return this.nodeType === 3 ? api.getText(this._id) : null; }
    set nodeValue(value) {
      if (this.nodeType === 3) this.textContent = value;
    }

    get textContent() { return api.getText(this._id); }
    set textContent(value) {
      // Nullable by spec: `el.textContent = null` empties the element rather
      // than writing the four characters "null".
      api.setText(this._id, value === null || value === undefined ? "" : String(value));
      if (observers.length === 0) return;
      record({
        type: "characterData", target: this, addedNodes: [], removedNodes: [],
        attributeName: null, oldValue: null,
      });
      childListRecord(this, [], []);
    }

    /// The text as *rendered*, which is what separates it from `textContent`.
    get innerText() { return api.innerText(this._id); }
    // The setter is not `textContent`: each line break in the string becomes
    // a real `<br>`, because innerText round-trips through the *rendered*
    // form — write "a\nb", read "a\nb" — and only a break element renders as
    // a break.
    set innerText(value) { this._replaceWithRenderedText(value, false); }
    get outerText() { return api.innerText(this._id); }
    set outerText(value) { this._replaceWithRenderedText(value, true); }
    _replaceWithRenderedText(value, replaceSelf) {
      const parts = String(value).split(/\r\n|\r|\n/);
      const nodes = [];
      parts.forEach((part, i) => {
        if (i > 0) nodes.push(document.createElement("br"));
        if (part !== "") nodes.push(document.createTextNode(part));
      });
      if (replaceSelf) {
        const parent = this.parentNode;
        if (!parent) {
          throw new DOMException(
            "outerText: the element has no parent",
            "NoModificationAllowedError",
          );
        }
        // An empty string still leaves a text node behind — the spec keeps a
        // merge point where the element was.
        if (nodes.length === 0) nodes.push(document.createTextNode(""));
        for (const node of nodes) parent.insertBefore(node, this);
        this.remove();
      } else {
        // Not `textContent = ""`, which parks an empty text node where the
        // content was — WPT counts the children afterwards.
        while (this.firstChild) this.removeChild(this.firstChild);
        for (const node of nodes) this.appendChild(node);
      }
    }

    /// DOM §4.2.3's first pre-insertion step: a node may not be put inside
    /// itself or inside anything it already contains.
    ///
    /// **Before the detach, which is the whole point.** `dom_api.rs` refuses the
    /// same thing, but by the time it is asked the node has been unlinked from
    /// its parent and the ancestor relationship it would have seen is gone — so
    /// the refusal there catches a raw primitive call and cannot catch this. The
    /// two are not redundant; they cover different halves of the same rule.
    _refuseIfAncestor(child) {
      if (!child || child.nodeType === 11) return;
      for (let at = this; at; at = at.parentNode) {
        if (at === child || at._id === child._id) {
          throw new DOMException(
            "the new child contains the parent it was being put inside",
            "HierarchyRequestError",
          );
        }
      }
    }

    appendChild(child) {
      this._refuseIfAncestor(child);
      // Inserting a fragment inserts its children and leaves the fragment
      // behind, which is the whole reason a fragment exists.
      if (child && child.nodeType === 11) {
        const moved = child.childNodes;
        for (const kid of moved) api.append(this._id, kid._id);
        if (child._children) child._children.length = 0;
        childListRecord(this, moved, []);
        notifyConnection(this);
        runInsertedScripts(this);
        return child;
      }
      detachFromParent(child);
      api.append(this._id, child._id);
      childListRecord(this, [child], []);
      notifyConnection(child);
      runInsertedScripts(child);
      return child;
    }
    insertBefore(child, anchor) {
      if (!anchor) return this.appendChild(child);
      this._refuseIfAncestor(child);
      // The spec's pre-insert step, and not a formality: without it the anchor
      // reaches blitz, which inserts relative to the anchor's parent and
      // unwraps it. A caller that passes a node from somewhere else is asking
      // for a NotFoundError and was getting a dead process.
      if (anchor.parentNode !== this) {
        throw new DOMException(
          "insertBefore: the reference node is not a child of this node",
          "NotFoundError",
        );
      }
      if (child && child.nodeType === 11) {
        for (const kid of child.childNodes) api.insertBefore(anchor._id, kid._id);
        if (child._children) child._children.length = 0;
        notifyConnection(this);
        runInsertedScripts(this);
        return child;
      }
      detachFromParent(child);
      api.insertBefore(anchor._id, child._id);
      childListRecord(this, [child], []);
      notifyConnection(child);
      runInsertedScripts(child);
      return child;
    }
    cloneNode(deep) {
      // A fragment clones to a fragment holding clones of its children, which
      // is the shape `appendChild` then expects.
      if (this.nodeType === 11) {
        const fragment = new DocumentFragment();
        if (deep) {
          for (const kid of this.childNodes) fragment.appendChild(kid.cloneNode(true));
        }
        return fragment;
      }
      // `is` is handed to `createElement` rather than copied with the other
      // attributes below, and the order is the whole of it: the wrapper is
      // built *during* creation and a customized built-in is chosen by `is` at
      // that moment, so a clone that learned its `is` a few lines later was
      // already a plain element and stayed one.
      const copy = this.nodeType === 3
        ? document.createTextNode(this.textContent)
        : document.createElement(this.tagName, { is: api.getAttr(this._id, "is") });
      if (this.nodeType === 1) {
        // **Every attribute, not two of them.** This copied `class` and
        // `style` and nothing else, so a cloned element lost its `id`, its
        // `href`, its `data-*` and every hook a page had put on it — and a
        // template cloned to be inserted came out stripped.
        for (const name of this.getAttributeNames()) {
          const value = api.getAttr(this._id, name);
          if (value !== null) copy.setAttribute(name, value);
        }
        // The *cloning steps* a form control carries: an `<input>` clone takes
        // the original's value and its dirty value flag, and a checkbox takes
        // its checkedness. Without them a cloned control comes back to its
        // markup default, which is not what the user had typed into it.
        if (this._value !== undefined) copy._value = this._value;
        if (this._checked !== undefined) copy._checked = this._checked;
        // `indeterminate` and `dirty checkedness` ride along too — WPT's
        // cloning-steps file checks each piece of control state by name.
        if (this.__h5iIndeterminate !== undefined) {
          copy.__h5iIndeterminate = this.__h5iIndeterminate;
        }
        if (deep) copy.innerHTML = this.innerHTML;
      }
      return copy;
    }
    /// The namespace lookup trio, with the answers an HTML document gives.
    lookupNamespaceURI(prefix) {
      const p = prefix === undefined || prefix === null || prefix === "" ? null : String(prefix);
      if (p === "xml") return "http://www.w3.org/XML/1998/namespace";
      if (p === "xmlns") return "http://www.w3.org/2000/xmlns/";
      if (this.nodeType === 1) {
        if (this._nsuri !== undefined) {
          // A createElementNS element knows its own namespace and prefix.
          if (p === (this._prefix ?? null)) return this._nsuri;
        } else if (p === null) {
          return "http://www.w3.org/1999/xhtml";
        }
        const parent = this.parentNode;
        return parent && parent.nodeType === 1 ? parent.lookupNamespaceURI(p) : null;
      }
      if (this.nodeType === 9) {
        return p === null ? "http://www.w3.org/1999/xhtml" : null;
      }
      // Fragments, doctypes, PIs: no element to inherit from.
      if (this.nodeType === 11 || this.nodeType === 10 || this.nodeType === 7) return null;
      const parent = this.parentNode;
      return parent ? parent.lookupNamespaceURI(p) : null;
    }
    lookupPrefix(namespace) {
      if (namespace === null || namespace === undefined || namespace === "") return null;
      const ns = String(namespace);
      if (this.nodeType === 1) {
        const mine = this._nsuri !== undefined ? this._nsuri : "http://www.w3.org/1999/xhtml";
        if (mine === ns && this._prefix) return this._prefix;
        const parent = this.parentNode;
        return parent && parent.nodeType === 1 ? parent.lookupPrefix(ns) : null;
      }
      return null;
    }
    isDefaultNamespace(namespace) {
      const ns = namespace === undefined || namespace === "" ? null : namespace;
      return this.lookupNamespaceURI(null) === ns;
    }

    get nextSibling() {
      const kids = this.parentNode ? this.parentNode.childNodes : [];
      const at = kids.findIndex((n) => n._id === this._id);
      return at >= 0 ? kids[at + 1] || null : null;
    }
    get previousSibling() {
      const kids = this.parentNode ? this.parentNode.childNodes : [];
      const at = kids.findIndex((n) => n._id === this._id);
      return at > 0 ? kids[at - 1] : null;
    }
    replaceChild(fresh, stale) {
      // Core DOM, and its absence is not a small gap: a hydrator that cannot
      // replace a node creates a new one beside it, which is how a page ends up
      // rendering its own content twice.
      if (!stale || stale.parentNode?._id !== this._id) {
        throw new TypeError("replaceChild: the node to replace is not a child of this node");
      }
      this.insertBefore(fresh, stale);
      this.removeChild(stale);
      return stale;
    }
    removeChild(child) {
      const leaving = collectCustom(child);
      api.removeNode(child._id);
      childListRecord(this, [], [child]);
      for (const node of leaving) fireDisconnected(node);
      return child;
    }
    remove() {
      const parent = this.parentNode;
      const leaving = collectCustom(this);
      api.removeNode(this._id);
      if (parent) childListRecord(parent, [], [this]);
      for (const node of leaving) fireDisconnected(node);
    }
    /// Insert siblings, and replace. All four are the same operation seen from
    /// four angles, and all four are what a framework calls to move a node.
    after(...items) {
      const parent = this.parentNode;
      if (!parent) return;
      const next = this.nextSibling;
      for (const item of items) {
        const node = item instanceof Node ? item : document.createTextNode(String(item));
        next ? parent.insertBefore(node, next) : parent.appendChild(node);
      }
    }
    before(...items) {
      const parent = this.parentNode;
      if (!parent) return;
      for (const item of items) {
        const node = item instanceof Node ? item : document.createTextNode(String(item));
        parent.insertBefore(node, this);
      }
    }
    replaceWith(...items) {
      this.before(...items);
      this.remove();
    }
    replaceChildren(...items) {
      for (const kid of this.childNodes) kid.remove();
      this.append(...items);
    }

    prepend(...items) {
      const first = this.firstChild;
      for (const item of items) {
        const node = item instanceof Node ? item : document.createTextNode(String(item));
        first ? this.insertBefore(node, first) : this.appendChild(node);
      }
    }
    append(...items) {
      for (const item of items) {
        this.appendChild(item instanceof Node ? item : document.createTextNode(String(item)));
      }
    }

    /// The HTML partial-update family: parse a string, drop what the caller
    /// asked dropped, and place the nodes with the positional verb the name
    /// carries. The safe spellings remove `<script>` outright; the unsafe
    /// ones keep it and run it only when `runScripts` says so — which is why
    /// each parsed script is pre-marked as started unless it is meant to run,
    /// keeping the generic insertion hook's hands off it.
    _insertParsedHTML(position, html, options, safe) {
      const host = document.createElement("div");
      host.innerHTML = String(html);
      const sanitizer = options && options.sanitizer;
      const removed = new Set(
        ((sanitizer && sanitizer.removeElements) || []).map((s) => String(s).toLowerCase()),
      );
      const banned = (el) =>
        (safe && el.tagName === "SCRIPT") || removed.has(el.tagName.toLowerCase());
      const nodes = [];
      for (const node of Array.from(host.childNodes)) {
        if (node.nodeType === 1) {
          if (banned(node)) continue;
          for (const kid of Array.from(node.querySelectorAll("*"))) {
            if (banned(kid)) kid.remove();
          }
        }
        nodes.push(node);
      }
      const runScripts = !safe && !!(options && options.runScripts);
      for (const node of nodes) {
        const scripts = [];
        if (node.tagName === "SCRIPT") scripts.push(node);
        if (typeof node.querySelectorAll === "function") {
          scripts.push(...node.querySelectorAll("script"));
        }
        for (const s of scripts) {
          if (!runScripts) s.__h5iScriptStarted = true;
        }
      }
      if (position === "append") this.append(...nodes);
      else if (position === "prepend") this.prepend(...nodes);
      else if (position === "before") this.before(...nodes);
      else if (position === "after") this.after(...nodes);
      else if (position === "replaceWith") this.replaceWith(...nodes);
    }

    contains(other) {
      for (let n = other; n; n = n.parentNode) if (n._id === this._id) return true;
      return false;
    }
    appendHTML(html, options) { this._insertParsedHTML("append", html, options, true); }
    appendHTMLUnsafe(html, options) { this._insertParsedHTML("append", html, options, false); }
    prependHTML(html, options) { this._insertParsedHTML("prepend", html, options, true); }
    prependHTMLUnsafe(html, options) { this._insertParsedHTML("prepend", html, options, false); }
    beforeHTML(html, options) { this._insertParsedHTML("before", html, options, true); }
    beforeHTMLUnsafe(html, options) { this._insertParsedHTML("before", html, options, false); }
    afterHTML(html, options) { this._insertParsedHTML("after", html, options, true); }
    afterHTMLUnsafe(html, options) { this._insertParsedHTML("after", html, options, false); }
    replaceWithHTML(html, options) { this._insertParsedHTML("replaceWith", html, options, true); }
    replaceWithHTMLUnsafe(html, options) { this._insertParsedHTML("replaceWith", html, options, false); }

    addEventListener(type, handler, options) {
      if (!handler) return;
      const capture = options === true || (options && options.capture) || false;
      const once = !!(options && options.once);
      // The same handler registered twice for the same type and phase is one
      // listener, as in a browser. Without this a page that re-runs its own
      // setup accumulates duplicates and every event fires N times.
      const already = listeners.some(
        (l) => l.id === this._id && l.type === String(type)
          && l.handler === handler && l.capture === capture,
      );
      if (already) return;
      // The passive-by-default set: touch and wheel listeners on the
      // document's scrolling surfaces cannot block scrolling, so their
      // `preventDefault` is ignored unless the page explicitly asked with
      // `passive: false` — the same rule browsers adopted for jank.
      let passive = options && typeof options === "object" && "passive" in options
        ? !!options.passive
        : undefined;
      if (passive === undefined) {
        const scrollBlocking = ["touchstart", "touchmove", "wheel", "mousewheel"];
        passive = scrollBlocking.includes(String(type))
          && (this._id === 0 || this.tagName === "HTML" || this.tagName === "BODY"
            || this.nodeType === 9);
      }
      listeners.push({ id: this._id, type: String(type), handler, capture, once, passive });
    }
    removeEventListener(type, handler) {
      for (let i = listeners.length - 1; i >= 0; i--) {
        const l = listeners[i];
        if (l.id === this._id && l.type === String(type) && l.handler === handler) {
          listeners.splice(i, 1);
        }
      }
    }
    dispatchEvent(event) { return dispatch(this, event); }
  }

  // A holder that is not in the document. Returning a `<div>` — which is what
  // this did before — injected a real element that the page never created,
  // breaking `.parent > .child` and the layout under it. Children live here
  // until the fragment is inserted, and then they move.
  class DocumentFragment {
    constructor() { this._children = []; this.nodeType = 11; }
    // The namespace trio: a fragment has no element to inherit from, so every
    // branch of the spec's algorithm lands on null. Spelled here because this
    // class does not extend Node and would otherwise answer with a TypeError,
    // which is a different (and wrong) fact.
    lookupNamespaceURI() { return null; }
    lookupPrefix() { return null; }
    isDefaultNamespace(ns) { return ns === null || ns === undefined || ns === ""; }
    get childNodes() { return this._children.slice(); }
    get firstChild() { return this._children[0] || null; }
    appendChild(child) { this._children.push(child); return child; }
    append(...items) {
      for (const item of items) {
        this.appendChild(item instanceof Node ? item : document.createTextNode(String(item)));
      }
    }
    prepend(...items) {
      for (const item of items.reverse()) {
        const node = item instanceof Node ? item : document.createTextNode(String(item));
        this._children.unshift(node);
      }
    }
    // The partial-update spellings a fragment can honour — a fragment has no
    // siblings, so the before/after family stays off it, as in the spec.
    appendHTML(html, options) { Node.prototype._insertParsedHTML.call(this, "append", html, options, true); }
    appendHTMLUnsafe(html, options) { Node.prototype._insertParsedHTML.call(this, "append", html, options, false); }
    prependHTML(html, options) { Node.prototype._insertParsedHTML.call(this, "prepend", html, options, true); }
    prependHTMLUnsafe(html, options) { Node.prototype._insertParsedHTML.call(this, "prepend", html, options, false); }
    removeChild(child) {
      const at = this._children.indexOf(child);
      if (at >= 0) this._children.splice(at, 1);
      return child;
    }
    // Searchable, because clone-query-fill-append is how a framework renders a
    // row and the query happens while the fragment is still detached. Each
    // child is its own scope: a fragment has no node of its own to search from.
    querySelector(sel) {
      for (const kid of this._children) {
        if (kid.nodeType !== 1) continue;
        if (kid.matches(sel)) return kid;
        const found = kid.querySelector(sel);
        if (found) return found;
      }
      return null;
    }
    querySelectorAll(sel) {
      const out = [];
      for (const kid of this._children) {
        if (kid.nodeType !== 1) continue;
        if (kid.matches(sel)) out.push(kid);
        out.push(...kid.querySelectorAll(sel));
      }
      return out;
    }
    get children() { return this._children.filter((n) => n.nodeType === 1); }
    get lastChild() { return this._children[this._children.length - 1] || null; }
    cloneNode(deep) {
      const copy = new DocumentFragment();
      if (deep) for (const kid of this._children) copy.appendChild(kid.cloneNode(true));
      return copy;
    }
  }

  /// A doctype node: three strings, `nodeType` 10, and nothing else.
  ///
  /// Detached-only, like a created PI: it is not backed by a node in the one
  /// real tree, because blitz holds the document's own doctype and a second
  /// one has nowhere to go. What a page does with these is *inspect* them —
  /// `createDocumentType` then reading name/publicId/systemId back — and the
  /// insertion path says no honestly rather than pretending.
  class DocumentTypeNode {
    /// `owner` is the document `createDocumentType` was called on — null for
    /// the page's own, which cannot be named here because `document` is built
    /// further down this file.
    constructor(name, publicId, systemId, owner) {
      this.name = name;
      this.publicId = publicId;
      this.systemId = systemId;
      this._owner = owner ?? null;
    }
    get nodeType() { return 10; }
    get nodeName() { return this.name; }
    get ownerDocument() { return this._owner ?? document; }
    get parentNode() { return null; }
    get childNodes() { return []; }
    get textContent() { return null; }
    // `null`, not `undefined`, and the difference was 76 of this file's 79
    // failures: DOM gives every node a `nodeValue`, and a doctype's is null.
    // Absent, it read as `undefined`, which is the one value an equality check
    // against null does not forgive.
    get nodeValue() { return null; }
    // The namespace trio answers null for a doctype in every branch, which is
    // the spec's own table.
    lookupNamespaceURI() { return null; }
    lookupPrefix() { return null; }
    isDefaultNamespace(ns) { return ns === null || ns === ""; }
  }

  /// An attribute as a node, which is what `attributes` and `createAttribute` hand back.
  class Attr {
    constructor(name, value, owner, namespace, prefix) {
      refuseExternal("Attr");
      this._name = String(name);
      this._value = value === undefined || value === null ? "" : String(value);
      this._owner = owner ?? null;
      this._ns = namespace ?? null;
      this._prefix = prefix ?? null;
    }
    get nodeType() { return 2; }
    get name() { return this._name; }
    get nodeName() { return this._name; }
    /// **A null prefix means the whole name is the local name**, colon or not:
    /// `createAttribute("a:b")` takes a *local* name, so its `localName` is
    /// `"a:b"`. Splitting on the colon regardless reported `"b"` with a null
    /// prefix, which is a pair that cannot both be true.
    get localName() {
      if (this._prefix === null) return this._name;
      return this._name.slice(this._name.indexOf(":") + 1);
    }
    get prefix() { return this._prefix; }
    get namespaceURI() { return this._ns; }
    get ownerElement() { return this._owner; }
    get ownerDocument() { return document; }
    // Legacy, and always true in a modern DOM: an attribute that exists was
    // specified.
    get specified() { return true; }
    get value() {
      if (this._owner) return api.getAttr(this._owner._id, this._name) ?? this._value;
      return this._value;
    }
    set value(v) {
      this._value = String(v);
      if (this._owner) this._owner.setAttribute(this._name, this._value);
    }
    get nodeValue() { return this.value; }
    set nodeValue(v) { this.value = v; }
    get textContent() { return this.value; }
    set textContent(v) { this.value = v; }
  }

  /// A processing instruction, which this engine's parser never produces —
  /// blitz reads `<?pi?>` as a comment — but pages construct to inspect, and
  /// the demand list counted 195 asks for exactly that.
  class ProcessingInstructionNode {
    constructor(target, data) {
      this.target = target;
      this.data = data;
    }
    get nodeType() { return 7; }
    get nodeName() { return this.target; }
    get textContent() { return this.data; }
    set textContent(v) { this.data = String(v); }
    get length() { return this.data.length; }
    get ownerDocument() { return document; }
    get parentNode() { return null; }
    get childNodes() { return []; }
    lookupNamespaceURI() { return null; }
    lookupPrefix() { return null; }
    isDefaultNamespace(ns) { return ns === null || ns === ""; }
  }

  /// The `CharacterData` interface, shared by text and comment nodes.
  ///
  /// `splitText` is the one that matters and the one that was missing:
  /// hydration splits a server-rendered text node when several vnodes share it,
  /// and a hydrator that cannot split creates fresh nodes instead — which is
  /// how preactjs.com rendered its version number twice and lost 147 lines of
  /// the page behind the mismatch.
  class CharacterData extends Node {
    get data() { return this.textContent; }
    set data(v) { this.textContent = v; }
    get length() { return this.data.length; }
    substringData(offset, count) { return this.data.substr(offset, count); }
    appendData(text) { this.data = this.data + String(text); }
    insertData(offset, text) {
      const current = this.data;
      this.data = current.slice(0, offset) + String(text) + current.slice(offset);
    }
    deleteData(offset, count) {
      const current = this.data;
      this.data = current.slice(0, offset) + current.slice(offset + count);
    }
    replaceData(offset, count, text) {
      const current = this.data;
      this.data = current.slice(0, offset) + String(text) + current.slice(offset + count);
    }
  }

  class Text extends CharacterData {
    /// `new Text(data)`, which is a page building a node, unless `FROM_ID` says
    /// this is the file wrapping one it already has.
    constructor(data, token) {
      super(token === FROM_ID
        ? data
        : api.createText(data === undefined ? "" : String(data)));
    }
    get wholeText() {
      // Adjacent text nodes read as one run, which is what the property means.
      let text = "";
      let first = this;
      while (first.previousSibling && first.previousSibling.nodeType === 3) {
        first = first.previousSibling;
      }
      for (let n = first; n && n.nodeType === 3; n = n.nextSibling) text += n.data;
      return text;
    }
    splitText(offset) {
      const current = this.data;
      const at = Math.max(0, Math.min(Number(offset) || 0, current.length));
      const tail = document.createTextNode(current.slice(at));
      this.data = current.slice(0, at);
      const parent = this.parentNode;
      if (parent) {
        const next = this.nextSibling;
        if (next) parent.insertBefore(tail, next);
        else parent.appendChild(tail);
      }
      return tail;
    }
  }

  // A real comment node, not a text node wearing a hat: a marker that showed up
  // in `textContent` would appear in the outline an agent reads.
  class Comment extends CharacterData {
    /// Same rule as `Text`: `new Comment(data)` is a page building a node.
    constructor(data, token) {
      super(token === FROM_ID
        ? data
        : api.createComment(data === undefined ? "" : String(data)));
    }
    get nodeType() { return 8; }
    get nodeValue() { return api.getText(this._id); }
  }

  class Element extends Node {
    // The class string for an element whose tag has no interface of its own.
    // Per-tag prototypes override this with their real name.
    get [Symbol.toStringTag]() { return "HTMLElement"; }
    get tagName() {
      // Uppercasing is an HTML-namespace privilege: `createElementNS`'s SVG
      // circle reports `circle`, and its prefixed name keeps the prefix.
      if (this._nsuri !== undefined && this._nsuri !== "http://www.w3.org/1999/xhtml") {
        return this._prefix ? `${this._prefix}:${this._localName}` : this._localName;
      }
      // Remembered on the wrapper, because a native call costs ~150 ns of dispatch however
      // little it does at the end of it — and this is the most-read property in the engine.
      return this._tag ?? (this._tag = api.tagName(this._id));
    }
    get nodeName() { return this.tagName; }
    get children() { return collection(this.childNodes.filter((n) => n.nodeType === 1), "HTMLCollection"); }

    getAttribute(name) { return api.getAttr(this._id, String(name)); }
    setAttribute(name, value) {
      // The old value is only wanted by an observer or a custom element, and
      // reading it is a call into the tree. Skipping it when nobody is watching
      // is most of what `setAttribute` used to cost.
      const watched = observers.length > 0 || isCustom(this);
      const previous = watched ? api.getAttr(this._id, String(name)) : null;
      api.setAttr(this._id, String(name), String(value));
      // `el.setAttribute("onclick", ...)` is the same handler the parser would
      // have compiled, arriving by another road.
      if (HANDLER_ATTR_SET.has(String(name).toLowerCase())) {
        const lowered = String(name).toLowerCase();
        const installed = this.__h5iInline ?? (this.__h5iInline = {});
        installed[lowered] = String(value);
        installInlineHandler(this, lowered, String(value));
      }
      if (watched) {
        const lowered = String(name).toLowerCase();
        recordAttribute(this, lowered, previous);
        fireAttributeChanged(this, lowered, previous, String(value));
      }
    }
    removeAttribute(name) {
      const watched = observers.length > 0 || isCustom(this);
      const previous = watched ? api.getAttr(this._id, String(name)) : null;
      api.removeAttr(this._id, String(name));
      if (watched) {
        const lowered = String(name).toLowerCase();
        recordAttribute(this, lowered, previous);
        fireAttributeChanged(this, lowered, previous, null);
      }
    }
    hasAttribute(name) { return api.getAttr(this._id, String(name)) !== null; }
    toggleAttribute(name, force) {
      const has = this.hasAttribute(name);
      const want = force === undefined ? !has : !!force;
      if (want) this.setAttribute(name, "");
      else this.removeAttribute(name);
      return want;
    }
    // Namespaces are not modelled — this engine parses HTML and nothing else —
    // so the namespace is dropped and the local name is used. Dropping it is
    // right for the case that actually occurs (`setAttributeNS(null, ...)`) and
    // honest for the rest: the attribute is set, under the name given.
    setAttributeNS(_namespace, name, value) { this.setAttribute(name, value); }
    getAttributeNS(_namespace, name) { return this.getAttribute(name); }
    removeAttributeNS(_namespace, name) { this.removeAttribute(name); }
    hasAttributeNS(_namespace, name) { return this.hasAttribute(name); }

    get id() { return this.getAttribute("id") || ""; }
    set id(v) { this.setAttribute("id", v); }
    get className() { return this.getAttribute("class") || ""; }
    set className(v) { this.setAttribute("class", v); }
    get classList() {
      // [SameObject]: the identical list every read — pages compare them.
      if (!this.__h5iClassList) {
        this.__h5iClassList =
          DOMTokenList._indexed(new DOMTokenList(this, "class"));
      }
      return this.__h5iClassList;
    }
    set classList(v) { this.setAttribute("class", String(v)); }
    get relList() {
      if (!this.__h5iRelList) {
        this.__h5iRelList =
          DOMTokenList._indexed(new DOMTokenList(this, "rel"));
      }
      return this.__h5iRelList;
    }

    // Setting a URL part rewrites the href it came from, which is how routing
    // code edits a link in place.
    set protocol(v) { this._setUrlPart("protocol", v); }
    set host(v) { this._setUrlPart("host", v); }
    set hostname(v) { this._setUrlPart("hostname", v); }
    set port(v) { this._setUrlPart("port", v); }
    set pathname(v) { this._setUrlPart("pathname", v); }
    set search(v) { this._setUrlPart("search", v); }
    set hash(v) { this._setUrlPart("hash", v); }
    _setUrlPart(part, value) {
      const raw = api.getAttr(this._id, "href");
      if (raw === null) return;
      const url = new URL(raw, currentAddress);
      url[part] = value;
      this.setAttribute("href", url.href);
    }

    _resolved(name) {
      const raw = api.getAttr(this._id, name);
      if (raw === null) return "";
      const parts = api.parseUrl(String(raw), currentAddress);
      return parts ? parts.href : raw;
    }

    // The pieces of that URL, which is how link-handling code decides whether a
    // click stays on the site. Empty on an element with no URL attribute, as in
    // a browser, rather than absent.
    get protocol() { return this._urlPart("protocol"); }
    get hostname() { return this._urlPart("hostname"); }
    get host() { return this._urlPart("host"); }
    get port() { return this._urlPart("port"); }
    get pathname() { return this._urlPart("pathname"); }
    get search() { return this._urlPart("search"); }
    get hash() { return this._urlPart("hash"); }
    get origin() { return this._urlPart("origin"); }
    _urlPart(part) {
      const raw = api.getAttr(this._id, "href") ?? api.getAttr(this._id, "src");
      if (raw === null) return "";
      const parts = api.parseUrl(String(raw), currentAddress);
      return parts ? parts[part] : "";
    }

    // HTML, always: this engine parses HTML and nothing else. `document` has no
    // such property in the DOM at all, which is why it is *defined as undefined*
    // there rather than left to report itself as a gap.
    get namespaceURI() {
      // Stored by `createElementNS`; everything the parser made is HTML.
      return this._nsuri !== undefined ? this._nsuri : "http://www.w3.org/1999/xhtml";
    }

    // What a reset button restores, and what `dirty` checks compare against.
    // The attribute for an input, the original text for a textarea — the two
    // places HTML keeps it.
    get defaultValue() {
      if (this.tagName === "TEXTAREA") return api.getText(this._id);
      return api.getAttr(this._id, "value") || "";
    }
    set defaultValue(v) {
      if (this.tagName === "TEXTAREA") this.textContent = String(v);
      else this.setAttribute("value", v);
    }

    // `el.scrollTop + el.clientHeight >= el.scrollHeight` is how every
    // "am I at the bottom" check is written, so all six come from one call and
    // agree with each other. Only the document actually scrolls here: nothing
    // in this engine clips and scrolls a subtree, and a scrollTop that can
    // never change is better reported as zero than invented.
    get scrollTop() { return (api.scrollMetrics(this._id) || [0])[0]; }
    set scrollTop(y) { api.setScrollTop(this._id, Number(y)); }
    get scrollLeft() { return (api.scrollMetrics(this._id) || [0, 0])[1]; }
    // Nothing here scrolls horizontally — no subtree clips and scrolls — so the
    // write is accepted and does nothing rather than throwing at a page that is
    // merely restoring a saved position.
    set scrollLeft(_x) {}
    get scrollHeight() { return (api.scrollMetrics(this._id) || [0, 0, 0])[2]; }
    get scrollWidth() { return (api.scrollMetrics(this._id) || [0, 0, 0, 0])[3]; }

    // `<template>.content` is a fragment view; `<meta content>` reflects the
    // attribute. Same property name, two unrelated meanings, both real.
    get content() {
      if (this.tagName === "TEMPLATE") return new TemplateContent(this._id);
      if (this.tagName === "META") return api.getAttr(this._id, "content") || "";
      return undefined;
    }

    get firstElementChild() { return this.children[0] || null; }
    get lastElementChild() { const c = this.children; return c[c.length - 1] || null; }
    get childElementCount() { return this.children.length; }
    get nextElementSibling() {
      for (let n = this.nextSibling; n; n = n.nextSibling) if (n.nodeType === 1) return n;
      return null;
    }
    get previousElementSibling() {
      for (let n = this.previousSibling; n; n = n.previousSibling) if (n.nodeType === 1) return n;
      return null;
    }

    // The live list, in source order. Enough of a `NamedNodeMap` for the three
    // things code does with it: iterate it, look a name up, and index it by
    // name. The last is not a nicety: jQuery 1.x reads
    // `div.attributes["onsubmit"].expando` and threw on the undefined, before
    // it had finished defining `$`.
    get attributes() {
      const node = this;
      const list = api.attrNames(this._id).map((name) => internal(
        () => new Attr(name, api.getAttr(node._id, name), node),
      ));
      list.getNamedItem = (name) =>
        list.find((a) => a.name === String(name).toLowerCase()) || null;
      for (const attr of list) {
        // A named property never shadows something the list already answers
        // to: `length` is the count, not the attribute of that name.
        if (attr.name in list) continue;
        Object.defineProperty(list, attr.name, { configurable: true, value: attr });
      }
      return list;
    }
    hasAttributes() { return api.attrNames(this._id).length > 0; }
    getAttributeNames() { return api.attrNames(this._id); }

    // Nothing is animating: this engine has no frames at rest, so there is
    // never an animation in progress to report. An empty list is what a browser
    // returns in that state, and it is the truth rather than a stub.
    getAnimations() { return []; }

    /// CSSOM-View's "would a person see this", which is the question a page
    /// asks before deciding an element is worth interacting with — and the one
    /// an agent asks for the same reason.
    ///
    /// No boxes means not rendered, which covers `display: none` here or on any
    /// ancestor without walking for it. `visibility` and `opacity` are opt-in
    /// and *do* need the walk, because both inherit their effect down the tree:
    /// a visible child of a `visibility: hidden` parent is still not shown.
    checkVisibility(options) {
      const wanted = options || {};
      if (!this.isConnected) return false;
      // No box means not rendered — except for `display: contents`, which the
      // algorithm carves out by name: the element generates no box of its own
      // and its children are still shown through it.
      if (this.getClientRects().length === 0
        && getComputedStyle(this).display !== "contents") return false;
      const checkVisibility = wanted.visibilityProperty ?? wanted.checkVisibilityCSS;
      const checkOpacity = wanted.opacityProperty ?? wanted.checkOpacity;
      if (!checkVisibility && !checkOpacity) return true;
      for (let node = this; node && node.nodeType === 1; node = node.parentNode) {
        const style = getComputedStyle(node);
        if (checkVisibility && style.visibility !== "visible") return false;
        if (checkOpacity && Number(style.opacity) === 0) return false;
      }
      return true;
    }

    // An iframe's document, which this engine does not load — see the note the
    // snapshot carries when a page has frames. Null is what a browser returns
    // for a frame it will not let you into, so a page's fallback path is the
    // right one to take.
    get contentDocument() { return null; }
    get contentWindow() { return null; }

    // Lowercase, always: this engine parses HTML, where the local name is
    // case-insensitive and canonically lower, while `tagName` is upper.
    get localName() {
      if (this._localName !== undefined) return this._localName;
      return this.tagName.toLowerCase();
    }
    /// Always null, and that is the answer rather than a gap: this engine
    /// parses HTML and nothing else (`document.contentType` says so), and every
    /// element in an HTML document is in the HTML namespace with no prefix.
    /// It was being *reported* as missing, which is the reverse of the mistake
    /// the reporting proxy exists to catch — naming as absent something no
    /// browser would answer differently.
    get prefix() { return this._prefix !== undefined ? this._prefix : null; }

    get contentEditable() {
      const raw = api.getAttr(this._id, "contenteditable");
      return raw === null ? "inherit" : (raw === "" ? "true" : raw);
    }
    set contentEditable(v) { this.setAttribute("contenteditable", v); }
    get isContentEditable() {
      for (let n = this; n; n = n.parentElement) {
        const raw = api.getAttr(n._id, "contenteditable");
        if (raw === "true" || raw === "") return true;
        if (raw === "false") return false;
      }
      return false;
    }

    get lang() { return api.getAttr(this._id, "lang") || ""; }
    set lang(v) { this.setAttribute("lang", v); }
    get title() { return api.getAttr(this._id, "title") || ""; }
    set title(v) { this.setAttribute("title", v); }

    // Bring the element into view for a screenshot or a live viewer. The
    // outline an agent reads covers the whole document either way, so this
    // changes what a *human* watching sees and nothing about what is readable.
    // Attach a shadow root, flattened into this element. See `ShadowRoot` for
    // what that costs and why it is the right trade for a reading engine.
    attachShadow(init) {
      if (this._shadow) {
        throw new Error("attachShadow: this element already has a shadow root");
      }
      const mode = String((init && init.mode) || "open");
      // Light children are taken out of the way first. A browser stops
      // rendering them once a shadow root exists unless they are slotted, and
      // leaving them would show a component's input and its output at once.
      const light = this.childNodes;
      for (const kid of light) detachFromParent(kid);

      const root = new ShadowRoot(this._id, mode);
      root._light = light;
      this._shadow = root;
      return root;
    }
    // Null for a closed root, as in a browser: the component asked for that,
    // and the flattening already leaks more than it should.
    get shadowRoot() {
      return this._shadow && this._shadow.mode === "open" ? this._shadow : null;
    }

    scrollIntoView() { api.scrollToNode(this._id); }
    // An element does not scroll here — nothing clips and scrolls a subtree —
    // so this moves the document, which is what the caller wanted when the
    // element was the document's own scroller.
    scrollTo(x, y) {
      const top = typeof x === "object" && x !== null ? x.top : y;
      api.setScrollTop(this._id, Number(top) || 0);
    }
    scrollBy(x, y) {
      const by = typeof x === "object" && x !== null ? x.top : y;
      api.setScrollTop(this._id, this.scrollTop + (Number(by) || 0));
    }

    // `<select>`. `selectedIndex` is how form code both reads and sets a
    // choice, and assigning it has to move the `selected` attribute or the
    // element and the DOM disagree about what is chosen.
    get selectedIndex() {
      const options = this.options;
      const at = options.findIndex((o) => o.selected);
      // A `<select>` with nothing marked selects its first option; -1 is only
      // right when there are no options at all.
      if (at >= 0) return at;
      return options.length ? 0 : -1;
    }
    set selectedIndex(index) {
      const options = this.options;
      const want = Number(index);
      options.forEach((option, at) => { option.selected = at === want; });
    }
    add(option, before) {
      if (before === undefined || before === null) return this.appendChild(option);
      const anchor = typeof before === "number" ? this.options[before] : before;
      return anchor ? this.insertBefore(option, anchor) : this.appendChild(option);
    }

    get options() { return this.querySelectorAll("option"); }

    // Real serialisation. Returning textContent here silently stripped every
    // tag, so `el.innerHTML = el.innerHTML` destroyed the subtree.
    get innerHTML() { return api.innerHtml(this._id); }
    set innerHTML(html) {
      api.setInnerHtml(this._id, String(html));
      // Markup written after load carries handlers too, and the lifecycle
      // sweep has already been and gone by the time a page does this.
      globalThis.__h5iInstallInlineHandlers(this);
    }

    /// `innerHTML`, plus the one thing `innerHTML` is specified *not* to do: turn `<template
    /// shadowrootmode>` into a real shadow root.
    setHTMLUnsafe(html) {
      api.setInnerHtml(this._id, String(html));
      adoptDeclarativeShadowRoots(this);
      globalThis.__h5iInstallInlineHandlers(this);
    }

    /// `innerHTML`, plus any shadow roots the caller asked to have serialised.
    getHTML(options) {
      const wants =
        options != null &&
        (options.serializableShadowRoots === true ||
          (Array.isArray(options.shadowRoots) && options.shadowRoots.length > 0));
      if (wants && this.shadowRoot) {
        api.unsupported("Element.getHTML({ serializableShadowRoots })");
      }
      return api.innerHtml(this._id);
    }
    // ---- The popover API -------------------------------------------------
    //
    // A self-contained feature and the largest one missing: 3,846 unpassed
    // subtests in `html/semantics/popovers` against 20 passing, because the
    // `popover` attribute reflected and nothing acted on it.
    //
    // **What is real here and what is not.** The state machine, the
    // exceptions, the event pair and the invoker wiring are all real: a page
    // can open a popover, be told why it could not, and observe the same
    // `beforetoggle`/`toggle` sequence a browser fires. What is missing is the
    // *top layer* — this engine has no separate paint layer, so an open
    // popover renders where it sits in the document rather than above
    // everything else, and light-dismiss-on-outside-click has no hit testing
    // to hang off. Those are rendering properties; the DOM contract is not,
    // and it is the half a page scripts against.
    _popoverState() {
      const kind = this.popover;
      return kind === null ? null : kind;
    }
    showPopover(options) {
      const kind = this._popoverState();
      if (kind === null) {
        throw new DOMException(
          "showPopover: this element has no `popover` attribute",
          "NotSupportedError",
        );
      }
      // A visibility mismatch is the one validity failure that stays silent:
      // showing what is already shown is a no-op, not an error. The order
      // matters — this returns *before* the connectivity check, because the
      // spec's "check popover validity" tests visibility second and never
      // throws for it.
      if (this.__h5iPopoverOpen) return;
      if (!this.isConnected) {
        throw new DOMException(
          "showPopover: the element is not connected",
          "InvalidStateError",
        );
      }
      // `beforetoggle` is cancelable on the way *open* only, which is the
      // asymmetry that lets a page veto a show and never a hide.
      const before = new ToggleEvent("beforetoggle", {
        cancelable: true, oldState: "closed", newState: "open",
      });
      this.dispatchEvent(before);
      if (before.defaultPrevented) return;
      // Re-checked after the handler: a `beforetoggle` listener may have
      // removed the element or opened it itself, and acting on the state we
      // read before running page script is how a double-open gets through.
      if (this.__h5iPopoverOpen || !this.isConnected) return;
      this.__h5iPopoverOpen = true;
      this.classList.add(POPOVER_OPEN_CLASS);
      // Auto popovers close their peers. Manual ones do not, which is the
      // whole difference between the two keywords.
      if (kind === "auto" || kind === "hint") {
        for (const other of api.queryAll("[popover]", 0).map(wrap)) {
          if (other && other !== this && other.__h5iPopoverOpen
            && other.popover !== "manual") {
            other.hidePopover();
          }
        }
      }
      this.dispatchEvent(new ToggleEvent("toggle", {
        oldState: "closed", newState: "open",
      }));
      void options;
    }
    hidePopover() {
      if (this._popoverState() === null) {
        throw new DOMException(
          "hidePopover: this element has no `popover` attribute",
          "NotSupportedError",
        );
      }
      // Hiding what is already hidden is a no-op — and that quietly covers the
      // disconnected case too, since removal closed it. Same silent-visibility
      // rule as `showPopover`.
      if (!this.__h5iPopoverOpen) return;
      this.dispatchEvent(new ToggleEvent("beforetoggle", {
        oldState: "open", newState: "closed",
      }));
      if (!this.__h5iPopoverOpen) return;
      this.__h5iPopoverOpen = false;
      this.classList.remove(POPOVER_OPEN_CLASS);
      this.dispatchEvent(new ToggleEvent("toggle", {
        oldState: "open", newState: "closed",
      }));
    }
    togglePopover(force) {
      // `force` is a boolean *or* an options object, and both spellings are in
      // use; the object form is what the current spec takes.
      const wanted = force && typeof force === "object" ? force.force : force;
      const open = !!this.__h5iPopoverOpen;
      if (wanted === true || (wanted === undefined && !open)) {
        if (!open) this.showPopover();
      } else if (wanted === false || wanted === undefined) {
        if (open) this.hidePopover();
      }
      return !!this.__h5iPopoverOpen;
    }

    get outerHTML() { return api.outerHtml(this._id); }
    set outerHTML(html) {
      // Replacing an element with its own markup, which is how a component
      // swaps itself out. The node is gone afterwards, as in a browser.
      const parent = this.parentNode;
      if (!parent) return;
      const host = document.createElement("div");
      host.innerHTML = String(html);
      const replacements = host.childNodes;
      for (const kid of replacements) parent.insertBefore(kid, this);
      this.remove();
    }

    // Deliberately not watched. A style declaration answers *any* CSS property
    // name by design — it is already a proxy over the dashed surface — so there
    // is no such thing as a name it is missing, and wrapping one proxy in
    // another defeats the `in` check the reporting one relies on.
    get style() { return new StyleDeclaration(inlineStyleSource(this)); }
    set style(text) { this.setAttribute("style", String(text)); }

    get dataset() {
      const node = this;
      return new Proxy({}, {
        get(_t, key) {
          if (typeof key !== "string") return undefined;
          const v = api.getAttr(node._id, "data-" + camelToDash(key));
          return v === null ? undefined : v;
        },
        set(_t, key, value) {
          api.setAttr(node._id, "data-" + camelToDash(String(key)), String(value));
          return true;
        },
        has(_t, key) {
          return api.getAttr(node._id, "data-" + camelToDash(String(key))) !== null;
        },
      });
    }

    querySelector(sel) { return withHasMarkers(sel, (t) => wrap(api.query(t, this._id))); }
    querySelectorAll(sel) { return withHasMarkers(sel, (t) => collection(api.queryAll(t, this._id).map(wrap))); }
    getElementsByTagName(tag) { return collection(api.queryAll(String(tag), this._id).map(wrap), "HTMLCollection"); }
    getElementsByClassName(cls) { return collection(api.queryAll("." + String(cls), this._id).map(wrap), "HTMLCollection"); }

    matches(sel) { return withHasMarkers(sel, (t) => api.matchesSelector(this._id, t)); }
    closest(sel) {
      for (let n = this; n; n = n.parentNode) {
        if (n.nodeType === 1 && n.matches(sel)) return n;
      }
      return null;
    }

    /// The placement rule the three `insertAdjacent*` methods share.
    ///
    /// They differ only in what they are handed — parsed markup, a text node,
    /// an element — so the four positions are worked out once here. Returns
    /// whether anything was inserted: `beforebegin` and `afterend` on a node
    /// with no parent are a no-op by spec rather than an error, and
    /// `insertAdjacentElement` has to return null for exactly that case.
    _insertAdjacent(position, nodes) {
      const where = String(position).toLowerCase();
      if (where === "beforeend") {
        for (const node of nodes) this.appendChild(node);
        return true;
      }
      if (where === "afterbegin") {
        const first = this.firstChild;
        for (const node of nodes) first ? this.insertBefore(node, first) : this.appendChild(node);
        return true;
      }
      if (where === "beforebegin" || where === "afterend") {
        const parent = this.parentNode;
        if (!parent) return false;
        if (where === "beforebegin") {
          for (const node of nodes) parent.insertBefore(node, this);
          return true;
        }
        const next = this.nextSibling;
        for (const node of nodes) next ? parent.insertBefore(node, next) : parent.appendChild(node);
        return true;
      }
      // A DOMException rather than the TypeError this used to throw: the spec
      // names this one, and a caller catching by type should find what the spec
      // told it to expect.
      throw new DOMException(
        "not one of beforebegin, afterbegin, beforeend, afterend: " + position,
        "SyntaxError",
      );
    }
    insertAdjacentHTML(position, html) {
      const host = document.createElement("div");
      api.setInnerHtml(host._id, String(html));
      this._insertAdjacent(position, [...host.childNodes]);
      host.remove();
    }
    insertAdjacentText(position, text) {
      this._insertAdjacent(position, [document.createTextNode(String(text))]);
    }
    insertAdjacentElement(position, element) {
      return this._insertAdjacent(position, [element]) ? element : null;
    }

    click() {
      // A real click on a checkbox toggles it *and* fires input then change,
      // in that order. A page that only listens for `change` — which is most
      // of them — sees nothing without this.
      const kind = this.type;
      if (this.tagName === "INPUT" && (kind === "checkbox" || kind === "radio")) {
        if (kind === "radio") {
          const name = this.name;
          if (name) {
            for (const other of document.querySelectorAll(`input[type=radio][name="${name}"]`)) {
              other.checked = false;
            }
          }
          this.checked = true;
        } else {
          this.checked = !this.checked;
        }
        dispatch(this, new MouseEvent("click", { bubbles: true }));
        dispatch(this, new InputEvent("input", { bubbles: true }));
        dispatch(this, new Event("change", { bubbles: true }));
        return;
      }
      // **A disabled control dispatches nothing.** `click()` on a disabled
      // button fired a click event here, so a page that disables a control to
      // stop it being used still saw it used — and the handler ran with the
      // form in whatever state the disabling was meant to protect.
      if (this.disabled) return;
      // An invoker acts *after* its click, and only if the click was not
      // cancelled: `preventDefault()` suppressing the default activation
      // behaviour is the whole reason this is here rather than inside the
      // dispatch. The event has to be held to be asked.
      const activation = new MouseEvent("click", { bubbles: true, cancelable: true });
      dispatch(this, activation);
      if (!activation.defaultPrevented) {
        this._runPopoverInvoker();
        this._runCommandInvoker();
        this._runFormButton();
      }
    }

    /// Activation behaviour for submit and reset buttons: clicking one *does
    /// something to the form*. Without this a `<button type=submit>` fired its
    /// click and nothing else — the form sat unsubmitted, which reads as a
    /// page that ignored the button.
    _runFormButton() {
      const tag = this.tagName;
      if (tag !== "BUTTON" && tag !== "INPUT") return;
      const form = this.form;
      if (!form) return;
      // A button's missing or invalid `type` is `submit`; an input's is `text`.
      const type = tag === "BUTTON"
        ? (() => {
            const raw = (api.getAttr(this._id, "type") || "").toLowerCase();
            return raw === "reset" || raw === "button" ? raw : "submit";
          })()
        : this.type;
      if (type === "submit" || type === "image") {
        try {
          form.requestSubmit(this);
        } catch {
          // A submitter the form refuses is a click that did nothing, not an
          // exception out of the page's own handler.
        }
      } else if (type === "reset") {
        form.reset();
      }
    }

    /// Activation behaviour for `popovertarget`, which is what makes the
    /// attribute do anything at all.
    ///
    /// Kept off the dispatch path deliberately: it runs after `click` has been
    /// delivered, so a listener that calls `preventDefault()` suppresses it,
    /// exactly as a browser's default activation behaviour works.
    _runPopoverInvoker() {
      if (this.tagName !== "BUTTON" && this.tagName !== "INPUT") return;
      // Only the button-like input types are invokers. A text field with a
      // `popovertarget` attribute is inert: clicking into it to type must not
      // toggle anything.
      if (this.tagName === "INPUT") {
        const type = (api.getAttr(this._id, "type") || "").toLowerCase();
        if (!["button", "reset", "submit", "image"].includes(type)) return;
      }
      const target = this.popoverTargetElement;
      if (!target || typeof target.togglePopover !== "function") return;
      if (target.popover === null || target.popover === undefined) return;
      const action = this.popoverTargetAction;
      try {
        if (action === "show") {
          if (!target.__h5iPopoverOpen) target.showPopover();
        } else if (action === "hide") {
          if (target.__h5iPopoverOpen) target.hidePopover();
        } else {
          target.togglePopover();
        }
      } catch {
        // An invoker aiming at something that cannot be opened is a no-op in a
        // browser, not an exception thrown out of a click handler.
      }
    }

    /// Activation behaviour for `commandfor`/`command` — the Invoker Commands
    /// API, which is `popovertarget` generalised: the button names an element
    /// and a verb, the element hears a `command` event, and the built-in verbs
    /// act on dialogs and popovers unless a listener cancels.
    _runCommandInvoker() {
      if (this.tagName !== "BUTTON") return;
      const invokee = this.commandForElement;
      if (!invokee) return;
      const command = this.command;
      if (command === "") return;
      const event = new CommandEvent("command", {
        cancelable: true, composed: true, command, source: this,
      });
      invokee.dispatchEvent(event);
      // A custom `--command` is *only* the event: its meaning belongs to the
      // page. The built-in verbs carry defaults, each gated on the kind of
      // element that understands it.
      if (event.defaultPrevented || command.startsWith("--")) return;
      try {
        if (invokee.tagName === "DIALOG") {
          if (command === "show-modal") {
            if (!invokee.hasAttribute("open")) invokee.showModal();
          } else if (command === "close") {
            invokee.close();
          } else if (command === "request-close") {
            invokee.requestClose();
          }
        } else if (invokee.popover !== null) {
          if (command === "toggle-popover") invokee.togglePopover();
          else if (command === "show-popover") invokee.showPopover();
          else if (command === "hide-popover") invokee.hidePopover();
        }
      } catch {
        // Same rule as the popover invoker: a verb aimed at something that
        // cannot take it is a no-op, not an exception out of a click handler.
      }
    }

    /// Move focus here, and fire what a browser fires.
    ///
    /// Both were empty, so `document.activeElement` never moved: a page that
    /// focused a field and then checked which field was focused got the wrong
    /// answer, and a form that advances focus as it validates got no signal at
    /// all. `focusin`/`focusout` bubble and `focus`/`blur` do not, which is the
    /// difference delegation depends on.
    focus() {
      if (focusedId === this._id) return;
      const previous = focusedId === null ? null : wrap(focusedId);
      focusedId = this._id;
      if (previous) {
        previous.dispatchEvent(new Event("blur", { bubbles: false }));
        previous.dispatchEvent(new Event("focusout", { bubbles: true }));
      }
      this.dispatchEvent(new Event("focus", { bubbles: false }));
      this.dispatchEvent(new Event("focusin", { bubbles: true }));
    }
    blur() {
      if (focusedId !== this._id) return;
      focusedId = null;
      this.dispatchEvent(new Event("blur", { bubbles: false }));
      this.dispatchEvent(new Event("focusout", { bubbles: true }));
    }

    // Answered from the layout the engine already computed. Returning zeros —
    // which is what this did before — sends a positioning library into a loop
    // that never converges.
    getBoundingClientRect() {
      const r = api.rect(this._id) || [0, 0, 0, 0];
      const [x, y, width, height] = r;
      return {
        x, y, width, height,
        top: y, left: x, right: x + width, bottom: y + height,
        toJSON() { return { x, y, width, height, top: y, left: x, right: x + width, bottom: y + height }; },
      };
    }
    // Empty for an element that generates no boxes — that emptiness *is* the
    // signal: `offsetWidth || getClientRects().length` is the visibility idiom
    // half the web uses, and a rect handed out for a `display: none` element
    // makes everything hidden read as visible. The `display` answer already
    // folds in ancestors (an unstyled node reports "none"), so one read covers
    // both "this is hidden" and "something above it is".
    getClientRects() {
      if (!this.isConnected) return [];
      if ((api.computedStyle(this._id, "display") || "") === "none") return [];
      return [this.getBoundingClientRect()];
    }
    get offsetWidth() { return this.getBoundingClientRect().width; }
    get offsetHeight() { return this.getBoundingClientRect().height; }
    /// Position relative to `offsetParent`, which for this engine is the page.
    ///
    /// A full implementation walks up for the nearest positioned ancestor and
    /// subtracts its border box. That is a real difference on a positioned
    /// subtree, and it is written down here rather than left to be discovered:
    /// what these return is the offset from the document, which is what
    /// `offsetParent` being the body means.
    get offsetTop() { return this.getBoundingClientRect().top; }
    get offsetLeft() { return this.getBoundingClientRect().left; }
    get offsetParent() {
      // Null for an element that is not rendered, which is the one case code
      // actually branches on.
      const display = api.computedStyle(this._id, "display") || "";
      if (display === "none" || !this.isConnected) return null;
      return wrap(api.query("body", 0));
    }
    // Not the bounding rect: for `documentElement` and `body` this is the
    // *viewport*, not the element's own height, and the bottom-of-page check
    // every page writes compares the two.
    get clientWidth() { return (api.scrollMetrics(this._id) || [0, 0, 0, 0, 0, 0])[5]; }
    get clientHeight() { return (api.scrollMetrics(this._id) || [0, 0, 0, 0, 0])[4]; }
  }

  // What `attachShadow` hands back.
  class ShadowRoot extends Element {
    constructor(hostId, mode) {
      super(hostId);
      this._mode = mode;
      this._light = [];
    }
    get nodeType() { return 11; }
    get nodeName() { return "#document-fragment"; }
    get mode() { return this._mode; }
    get host() { return wrap(this._id); }
    get activeElement() { return null; }
    get styleSheets() { return []; }
    // The host is where content actually lives, so `innerHTML` on the root and
    // on the host are the same string — and setting it re-runs projection,
    // because the `<slot>` a component declares usually arrives with it.
    set innerHTML(html) {
      api.setInnerHtml(this._id, String(html));
      this._project();
    }
    get innerHTML() { return api.innerHtml(this._id); }
    setHTMLUnsafe(html) {
      api.setInnerHtml(this._id, String(html));
      adoptDeclarativeShadowRoots(this);
      this._project();
    }
    // Same rule as the host's, and for the same reason: this root *is* the
    // host, so its serialisation is the host's content.
    getHTML(options) { return Element.prototype.getHTML.call(this, options); }
    appendChild(child) {
      const out = Element.prototype.appendChild.call(this, child);
      this._project();
      return out;
    }
    // Put the light children where the component said they should go. One
    // unnamed slot, which is what the overwhelming majority declare; a page
    // using named slots keeps its light content out of the way instead, which
    // is a gap worth reporting rather than guessing at.
    _project() {
      if (this._light.length === 0) return;
      const slot = wrap(api.query("slot", this._id));
      if (!slot) return;
      const pending = this._light;
      this._light = [];
      for (const node of pending) slot.appendChild(node);
    }
  }

  // What `<template>.content` hands back.
  class TemplateContent extends Element {
    get nodeType() { return 11; }
    get nodeName() { return "#document-fragment"; }
    // The one place a template's children *are* visible. `Element` hides them
    // for the element itself, per the rule below, and this is the fragment
    // they officially belong to.
    get childNodes() { return api.children(this._id).map(wrap); }
    get children() { return collection(this.childNodes.filter((n) => n.nodeType === 1), "HTMLCollection"); }
  }

  // `el.onclick = fn` is the other way to bind a handler, and plenty of
  // generated code still uses it. Defined rather than enumerated by hand so the
  // property and `addEventListener` cannot disagree about what is registered.
  // Attributes that are nothing but a string on the element. Defined from a
  // table rather than written out twice, so a getter can never exist without
  // its setter — which is the bug this table was written to end: in a module,
  // which is strict, assigning to a getter-only property *throws*, and a
  // classic-script test cannot see it because sloppy mode swallows it.
  /// The content attributes every element reflects, because they are global.
  const NULL_IS_EMPTY = { nullAsEmpty: true };
  // The form-submission enumerations. `enctype` on the form falls back to
  // urlencoded whether missing or garbage; the per-button `form*` overrides
  // report "" when absent — absent means "defer to the form" — but garbage
  // still snaps to the invalid-value default.
  const ENCTYPE_KEYWORDS = [
    "application/x-www-form-urlencoded", "multipart/form-data", "text/plain",
  ];
  const ENCTYPE = {
    keywords: ENCTYPE_KEYWORDS,
    missing: "application/x-www-form-urlencoded",
    invalid: "application/x-www-form-urlencoded",
  };
  const FORM_ENCTYPE = {
    keywords: ENCTYPE_KEYWORDS,
    missing: "",
    invalid: "application/x-www-form-urlencoded",
  };
  const FORM_METHOD = {
    keywords: ["get", "post", "dialog"], missing: "", invalid: "get",
  };
  // Shared by every element that carries `referrerpolicy`: same keywords, and
  // garbage reads as the empty string (the default policy), not as itself.
  const REFERRER_POLICY = { keywords: [
    "", "no-referrer", "no-referrer-when-downgrade", "same-origin",
    "origin", "strict-origin", "origin-when-cross-origin",
    "strict-origin-when-cross-origin", "unsafe-url"] };

  /// `action` and `formAction` answer with the *document's* address when the
  /// attribute is missing or empty, rather than with "". A form whose action
  /// reads "" submits somewhere different from one that reads the page's URL,
  /// so this is a behaviour difference and not a formatting one.
  const DOCUMENT_URL_WHEN_EMPTY = { emptyIsDocumentUrl: true };

  /// `crossOrigin` is a **nullable** enumerated attribute, and every element that has it has
  /// the same one.
  const PRELOAD = ["preload", "preload", "enumerated", {
    keywords: ["none", "metadata", "auto"], missing: "auto", invalid: "auto",
    aliases: { "": "auto" },
  }];
  const LOADING = ["loading", "loading", "enumerated", {
    keywords: ["lazy", "eager"], missing: "eager", invalid: "eager",
  }];

  const CROSS_ORIGIN = ["crossOrigin", "crossorigin", "enumerated", {
    keywords: ["anonymous", "use-credentials"],
    missing: null,
    invalid: "anonymous",
  }];

  const REFLECTED_ATTRIBUTES = {
    dir: "dir",
    slot: "slot",
    accessKey: "accesskey",
  };  for (const [property, attribute] of Object.entries(REFLECTED_ATTRIBUTES)) {
    Object.defineProperty(Element.prototype, property, {
      configurable: true,
      get() { return api.getAttr(this._id, attribute) || ""; },
      set(value) { this.setAttribute(attribute, value); },
    });
  }

  /// Reflect an IDL property onto a content attribute, with the *type* the spec gives it.
  function reflect(proto, idl, content, type = "string", options = {}) {
    const parseInteger = (raw) => {
      // The spec's rules for parsing integers, which are not `Number()`:
      // leading whitespace is skipped, trailing garbage ends the number, and
      // anything else is a failure rather than a NaN to paper over.
      const match = /^[ \t\n\f\r]*([+-]?[0-9]+)/.exec(raw ?? "");
      if (!match) return null;
      const value = Number(match[1]);
      if (!Number.isSafeInteger(value)) return null;
      // **`-0` is not a value a reflection may report.** `Number("-0")` is
      // negative zero, and IDL longs are integers — there is one zero. It
      // matters because testharness compares with `Object.is` semantics, so
      // `-0` fails an `assert_equals(0)` that looks like it should pass, and
      // `tabIndex` is reflected on *every* element: one `setAttribute("-0")`
      // subtest per element in every `reflection-*.html` file.
      if (value === 0) return 0;
      // Out of the 32-bit range is "not a valid integer" for a reflection, not
      // a large number: `marquee.hspace = 2147483648` reads back as the
      // default in a browser and read back as 2147483648 here.
      if (value < -2147483648 || value > 2147483647) return null;
      return value;
    };
    const get = {
      string() { return api.getAttr(this._id, content) ?? ""; },
      // Nullable, unlike a plain DOMString: the ARIA properties report `null`
      // for an attribute that is absent rather than an empty string, and a test
      // that distinguishes the two is testing something real.
      nullable() { return api.getAttr(this._id, content); },
      bool() { return api.getAttr(this._id, content) !== null; },
      long() {
        const value = parseInteger(api.getAttr(this._id, content));
        if (value === null) return options.default ?? 0;
        // "Limited to only non-negative numbers": a negative in the markup is
        // not a value, it is the default — `maxlength="-36"` reads -1.
        if (options.nonNegative && value < 0) return options.default ?? 0;
        return value;
      },
      ulong() {
        const value = parseInteger(api.getAttr(this._id, content));
        if (value === null || value < 0) return options.default ?? 0;
        // "Limited to only positive numbers": zero is out of range, so
        // `size="0"` falls back rather than reporting an impossible size.
        if (options.positive && value === 0) return options.default ?? 0;
        // "Clamped to the range": `colgroup.span` reads 0 as 1 and the
        // 32-bit maximum as 1000 — clamping, unlike the rules above, keeps
        // the out-of-range value's *direction*.
        if (options.clamp) {
          const [lo, hi] = options.clamp;
          return Math.min(Math.max(value, lo), hi);
        }
        // No range check here: `parseInteger` already answers `null` for
        // anything outside the 32-bit range, so a guard at this point is
        // unreachable. One stood here claiming that a *clamped* attribute
        // pinned to its ceiling instead — it did not, and does not: because the
        // rejection happens in the parse, `colgroup.span = "2147483648"` reads
        // 1 rather than 1000. That is a real bug, older than this comment, and
        // it lives in `parseInteger` rather than here.
        return value;
      },
      // A reflected `double`, for `<meter>` and `<progress>`. Not an integer
      // parse: `min`, `max`, `low`, `high`, `optimum` and `value` are all
      // floating point, and rounding them would make a half-full meter read as
      // empty.
      double() {
        const raw = api.getAttr(this._id, content);
        if (raw === null) return options.default ?? 0;
        const value = Number(String(raw).trim());
        if (!Number.isFinite(value)) return options.default ?? 0;
        return value;
      },
      enumerated() {
        const raw = api.getAttr(this._id, content);
        // `in` rather than `??`, because `null` is a real missing-value default
        // — `crossOrigin` reports null for an absent attribute — and `??` would
        // quietly turn that into "".
        if (raw === null) return "missing" in options ? options.missing : "";
        // ASCII lowercase, not `toLowerCase()`: keyword matching is defined
        // over ASCII, and Unicode lowering is *wider* — it folds U+212A
        // (kelvin) to "k" and U+017F (long s) to "s", making garbage match.
        // WPT plants exactly those characters to catch it.
        const lower = String(raw).replace(/[A-Z]/g, (c) => c.toLowerCase());
        // Aliases first: a keyword can have more than one spelling that maps to
        // the same state, and the empty string is the one that matters —
        // `<div contenteditable>` is the "true" state, so an implementation
        // that only matched the literal keywords reported "inherit" for the
        // most common way anyone writes it.
        if (options.aliases && lower in options.aliases) return options.aliases[lower];
        const found = options.keywords.find((word) => word.toLowerCase() === lower);
        if (found !== undefined) return found;
        return "invalid" in options ? options.invalid : "";
      },
      url() {
        const raw = api.getAttr(this._id, content);
        // Some URL reflections answer with the *document's* address when the
        // attribute is missing or empty, rather than with "": `form.action`
        // and `input.formAction` are the ones that matter, and a form whose
        // action reads "" submits somewhere different from one that reads the
        // page's own URL.
        if (options.emptyIsDocumentUrl && (raw === null || raw === "")) {
          return currentAddress ?? "";
        }
        if (raw === null) return "";
        // `currentAddress`, matching `_resolved` above: only Document carries a
        // `baseURI`, and resolving against `undefined` would hand back the raw
        // attribute for every relative URL while looking like it resolved.
        const parts = api.parseUrl(String(raw), currentAddress);
        // An unparseable URL reflects as the literal attribute, which is what a
        // browser does and is more useful than an empty string when debugging.
        return parts ? parts.href : String(raw);
      },
    }[type];
    const set = type === "bool"
      ? function (on) {
        if (on) this.setAttribute(content, "");
        else this.removeAttribute(content);
      }
      : type === "long" || type === "ulong"
        ? function (value) {
          const number = Number(value);
          let written = Number.isFinite(number) ? Math.trunc(number) : 0;
          // WebIDL conversion happens *before* the reflection rules see the
          // value: a long wraps modulo 2^32 into signed range (`|0`), an
          // unsigned long wraps into unsigned range (`>>>0`) and anything the
          // attribute still cannot hold — above 2147483647, which includes
          // every negative after wrapping — becomes the default. Without the
          // wrap, `img.width = 2147483648` wrote "2147483648" into markup
          // where a browser writes "0".
          if (type === "long") {
            written |= 0;
          } else {
            written >>>= 0;
            if (written > 2147483647) written = options.default ?? 0;
          }
          this.setAttribute(content, String(written));
        }
        : type === "double"
          ? function (value) {
            const number = Number(value);
            this.setAttribute(content, String(Number.isFinite(number) ? number : 0));
          }
          : function (value) {
            // `null` on a nullable reflection removes the attribute — and an
            // enumerated attribute whose missing-value default is `null` is
            // nullable too (`crossOrigin`), where `undefined` also means "no
            // value" rather than the string "undefined".
            const nullableEnum =
              type === "enumerated" && (options.nullable || options.missing === null);
            if (value === null && type === "nullable") this.removeAttribute(content);
            else if (nullableEnum && (value === null || value === undefined)) {
              this.removeAttribute(content);
            }
            // `[LegacyNullToEmptyString]`, which the legacy presentational
            // attributes carry: `body.bgColor = null` writes "" and not the
            // string "null". Marked per attribute rather than guessed at,
            // because everywhere *else* `null` really does stringify —
            // `el.dir = null` writes "null" and a browser agrees.
            else if (value === null && options.nullAsEmpty) this.setAttribute(content, "");
            else this.setAttribute(content, String(value));
          };
    // WebIDL's brand check: reading `HTMLElement.prototype.title` — the
    // accessor invoked with the prototype itself as `this` — throws TypeError
    // rather than reaching for an `_id` the prototype does not have.
    // idlharness probes exactly this on every attribute of every interface.
    const guardedGet = function () {
      if (!this || this._id === undefined) {
        throw new TypeError(`Illegal invocation: ${idl} needs an element`);
      }
      return get.call(this);
    };
    const guardedSet = function (value) {
      if (!this || this._id === undefined) {
        throw new TypeError(`Illegal invocation: ${idl} needs an element`);
      }
      return set.call(this, value);
    };
    // WebIDL names accessor functions `get title`/`set title` — the same
    // spelling class syntax produces — and idlharness reads the name back.
    Object.defineProperty(guardedGet, "name", { value: `get ${idl}` });
    Object.defineProperty(guardedSet, "name", { value: `set ${idl}` });
    Object.defineProperty(proto, idl, {
      configurable: true,
      // WebIDL interface members are enumerable, which idlharness also
      // checks; class syntax and defineProperty both default the other way.
      enumerable: true,
      get: guardedGet,
      set: guardedSet,
    });
  }

  // The attributes every HTML element carries. `hidden` and `tabIndex` were
  // the only two of these that existed, hand-written, before WPT was pointed
  // at the engine.
  reflect(Element.prototype, "hidden", "hidden", "bool");
  reflect(Element.prototype, "autofocus", "autofocus", "bool");
  reflect(Element.prototype, "inert", "inert", "bool");
  // Three booleans whose attribute form is a keyword pair rather than
  // presence: `translate` speaks yes/no, `autocorrect` on/off, `draggable`
  // true/false — and each has its own reading of "absent".
  Object.defineProperty(Element.prototype, "translate", {
    configurable: true, enumerable: true,
    get: Object.defineProperty(function () {
      if (!this || this._id === undefined) throw new TypeError("Illegal invocation");
      return (api.getAttr(this._id, "translate") || "").toLowerCase() !== "no";
    }, "name", { value: "get translate" }),
    set: Object.defineProperty(function (value) {
      if (!this || this._id === undefined) throw new TypeError("Illegal invocation");
      this.setAttribute("translate", value ? "yes" : "no");
    }, "name", { value: "set translate" }),
  });
  Object.defineProperty(Element.prototype, "autocorrect", {
    configurable: true, enumerable: true,
    get: Object.defineProperty(function () {
      if (!this || this._id === undefined) throw new TypeError("Illegal invocation");
      return (api.getAttr(this._id, "autocorrect") || "").toLowerCase() !== "off";
    }, "name", { value: "get autocorrect" }),
    set: Object.defineProperty(function (value) {
      if (!this || this._id === undefined) throw new TypeError("Illegal invocation");
      this.setAttribute("autocorrect", value ? "on" : "off");
    }, "name", { value: "set autocorrect" }),
  });
  Object.defineProperty(Element.prototype, "draggable", {
    configurable: true, enumerable: true,
    get: Object.defineProperty(function () {
      if (!this || this._id === undefined) throw new TypeError("Illegal invocation");
      const raw = (api.getAttr(this._id, "draggable") || "").toLowerCase();
      if (raw === "true") return true;
      if (raw === "false") return false;
      // The absent default is per element: images and links drag, text does not.
      return this.tagName === "IMG"
        || (this.tagName === "A" && api.getAttr(this._id, "href") !== null);
    }, "name", { value: "get draggable" }),
    set: Object.defineProperty(function (value) {
      if (!this || this._id === undefined) throw new TypeError("Illegal invocation");
      this.setAttribute("draggable", value ? "true" : "false");
    }, "name", { value: "set draggable" }),
  });
  Object.defineProperty(Element.prototype, "accessKeyLabel", {
    configurable: true, enumerable: true,
    get: Object.defineProperty(function () {
      if (!this || this._id === undefined) throw new TypeError("Illegal invocation");
      // No keyboard here, so no label — the honest constant.
      return "";
    }, "name", { value: "get accessKeyLabel" }),
  });
  // `tabIndex` has no single default: an element the user can reach with the
  // keyboard reports 0, everything else -1. Answering -1 for a link or a button
  // tells a page nothing is focusable, which is the opposite of true.
  const FOCUSABLE_BY_DEFAULT = new Set([
    "A", "AREA", "BUTTON", "INPUT", "SELECT", "TEXTAREA", "IFRAME", "OBJECT",
    "SUMMARY", "AUDIO", "VIDEO",
  ]);
  Object.defineProperty(Element.prototype, "tabIndex", {
    configurable: true,
    get() {
      const raw = api.getAttr(this._id, "tabindex");
      if (raw !== null) {
        const match = /^[ \t\n\f\r]*([+-]?[0-9]+)/.exec(raw);
        if (match) {
          const value = Number(match[1]);
          // Its own parser, so it needs the same two rules `parseInteger` has:
          // one zero, and nothing outside the 32-bit range. `tabindex="-0"`
          // reported `-0`, which fails `assert_equals(0)` on every element in
          // the suite.
          if (value === 0) return 0;
          if (Number.isSafeInteger(value)
            && value >= -2147483648 && value <= 2147483647) return value;
        }
      }
      if (!FOCUSABLE_BY_DEFAULT.has(this.tagName)) return -1;
      // A link is only focusable if it actually links somewhere.
      if ((this.tagName === "A" || this.tagName === "AREA")
        && api.getAttr(this._id, "href") === null) return -1;
      return 0;
    },
    set(value) { this.setAttribute("tabindex", String(Math.trunc(Number(value)) || 0)); },
  });
  reflect(Element.prototype, "accessKey", "accesskey");
  reflect(Element.prototype, "slot", "slot");
  reflect(Element.prototype, "nonce", "nonce");
  reflect(Element.prototype, "dir", "dir", "enumerated", { keywords: ["ltr", "rtl", "auto"] });
  reflect(Element.prototype, "contentEditable", "contenteditable", "enumerated", {
    keywords: ["true", "false", "plaintext-only"],
    aliases: { "": "true" },
    missing: "inherit",
    invalid: "inherit",
  });
  reflect(Element.prototype, "autocapitalize", "autocapitalize", "enumerated", {
    keywords: ["none", "off", "on", "sentences", "words", "characters"],
  });
  reflect(Element.prototype, "inputMode", "inputmode", "enumerated", {
    keywords: ["none", "text", "tel", "url", "email", "numeric", "decimal", "search"],
  });
  reflect(Element.prototype, "enterKeyHint", "enterkeyhint", "enumerated", {
    keywords: ["enter", "done", "go", "next", "previous", "search", "send"],
  });
  // Nullable, and three keywords rather than two: an element with no `popover`
  // attribute reports `null`, which is how a page asks "is this a popover at
  // all" — reporting "" made every element look like one.
  reflect(Element.prototype, "popover", "popover", "enumerated", {
    keywords: ["auto", "manual", "hint"],
    missing: null,
    invalid: "manual",
    // `<div popover>` is the common spelling and its value is the empty
    // string, which maps to the *auto* state. Without the alias it fell
    // through to `invalid` and every bare popover reported "manual" — the
    // one state that does not close its peers, so they all stacked up.
    aliases: { "": "auto" },
  });

  // ARIA, which reflects mechanically: every one of these is `aria-` followed
  // by the rest of the name lowercased, with no word separator —
  // `ariaHasPopup` is `aria-haspopup`, not `aria-has-popup`. Written as a list
  // rather than as forty pairs because the mapping has no exceptions.
  for (const name of [
    "ariaAtomic", "ariaAutoComplete", "ariaBrailleLabel", "ariaBrailleRoleDescription",
    "ariaBusy", "ariaChecked", "ariaColCount", "ariaColIndex", "ariaColIndexText",
    "ariaColSpan", "ariaCurrent", "ariaDescription", "ariaDisabled", "ariaExpanded",
    "ariaHasPopup", "ariaHidden", "ariaInvalid", "ariaKeyShortcuts", "ariaLabel",
    "ariaLevel", "ariaLive", "ariaModal", "ariaMultiLine", "ariaMultiSelectable",
    "ariaOrientation", "ariaPlaceholder", "ariaPosInSet", "ariaPressed", "ariaReadOnly",
    "ariaRelevant", "ariaRequired", "ariaRoleDescription", "ariaRowCount", "ariaRowIndex",
    "ariaRowIndexText", "ariaRowSpan", "ariaSelected", "ariaSetSize", "ariaSort",
    "ariaValueMax", "ariaValueMin", "ariaValueNow", "ariaValueText",
  ]) {
    reflect(Element.prototype, name, "aria-" + name.slice(4).toLowerCase(), "nullable");
  }
  reflect(Element.prototype, "role", "role", "nullable");

  // The ARIA properties that are *enumerated* rather than free strings — with the spec's own
  // table, not a uniform rule.
  const EMPTY_IS_FALSE = { "": "false" };
  const EMPTY_IS_NULL = { "": null };

  /// The three option objects the ARIA table repeats. Named once, because
  /// fifteen copies of the same literal is fifteen copies in the parse budget
  /// and one place to get the table wrong instead of fifteen.
  const ARIA_FALSE = { missing: "false", invalid: "false", aliases: EMPTY_IS_FALSE };
  const ARIA_NULL = { missing: null, invalid: null, aliases: EMPTY_IS_NULL };
  const ARIA_FALSE_TRUE = { missing: "false", invalid: "true", aliases: EMPTY_IS_FALSE };
  for (const [name, keywords, options] of [
    ["ariaAtomic", ["true", "false"],
      { missing: null, invalid: "false", aliases: EMPTY_IS_FALSE }],
    ["ariaAutoComplete", ["inline", "list", "both", "none"],
      { missing: "none", invalid: "none" }],
    ["ariaBusy", ["true", "false"],
      ARIA_FALSE],
    ["ariaChecked", ["true", "false", "mixed"],
      ARIA_NULL],
    ["ariaCurrent", ["page", "step", "location", "date", "time", "true", "false"],
      ARIA_FALSE_TRUE],
    ["ariaDisabled", ["true", "false"],
      ARIA_FALSE],
    ["ariaExpanded", ["true", "false"],
      ARIA_NULL],
    ["ariaHasPopup", ["true", "false", "menu", "dialog", "listbox", "tree", "grid"],
      { missing: null, invalid: "false" }],
    ["ariaHidden", ["true", "false"],
      ARIA_FALSE],
    ["ariaInvalid", ["true", "false", "spelling", "grammar"],
      ARIA_FALSE_TRUE],
    ["ariaLive", ["polite", "assertive", "off"],
      { missing: "off", invalid: "off" }],
    ["ariaModal", ["true", "false"],
      ARIA_FALSE],
    ["ariaMultiLine", ["true", "false"],
      ARIA_FALSE],
    ["ariaMultiSelectable", ["true", "false"],
      ARIA_FALSE],
    ["ariaOrientation", ["horizontal", "vertical"],
      ARIA_NULL],
    ["ariaPressed", ["true", "false", "mixed"],
      ARIA_NULL],
    ["ariaReadOnly", ["true", "false"],
      ARIA_FALSE],
    ["ariaRequired", ["true", "false"],
      ARIA_FALSE],
    ["ariaSelected", ["true", "false"],
      ARIA_NULL],
    ["ariaSort", ["ascending", "descending", "other", "none"],
      { missing: "none", invalid: "none" }],
  ]) {
    reflect(Element.prototype, name, "aria-" + name.slice(4).toLowerCase(),
      "enumerated", { keywords, nullable: true, ...options });
  }

  // ── per-tag interfaces ───────────────────────────────────────────────────
  //
  // A browser has HTMLAnchorElement, HTMLTableCellElement and eighty more, and
  // the split is not cosmetic. `colSpan` belongs to <td> and <th>; `span` to
  // <col> and <colgroup>; `scrollAmount` to <marquee> and nothing else. Hanging
  // all of them on one Element would make `"colSpan" in div` true, which is the
  // same lie the removed `missingApi` stubs told: feature detection asks before
  // it uses, and gets sent down a branch a real browser never takes.
  //
  // Each entry is [idl, content, type, options] and reads as the spec's
  // reflection table does. Names Element already defines — href, src, name,
  // type, disabled, value, checked, selected — are deliberately absent: those
  // carry behaviour beyond reflection, and `defaultChecked`, `defaultSelected`
  // and `defaultValue` are the spec's names for the reflecting half.
  /// Every name `reflect`'s type switch answers to.
  ///
  /// **Completeness is load-bearing**, not tidiness: a type missing from this
  /// set is read as a *content attribute name* by the shorthand below, so
  /// `["link", "string", NULL_IS_EMPTY]` silently reflected an attribute called
  /// "string" and handed the options object in as the type. `string` was left
  /// out of the first version and took 76 subtests of `reflection-sections`
  /// with it. Add a type here in the same commit that adds it there.
  const REFLECT_TYPES = new Set([
    "string", "bool", "ulong", "long", "double", "url", "enumerated", "tokenlist",
  ]);

  const REFLECTIONS = {
    html: ["HTMLHtmlElement", ["version"]],
    head: ["HTMLHeadElement", []],
    title: ["HTMLTitleElement", []],
    base: ["HTMLBaseElement", ["target"]],
    link: ["HTMLLinkElement", [
      "rel", "media", "hreflang",
      "integrity", ["imageSrcset", "imagesrcset"],
      ["imageSizes", "imagesizes"], "charset", "rev",
      "target",
      ["as", "enumerated", { keywords: [
        "fetch", "audio", "audioworklet", "document", "embed", "font", "frame",
        "iframe", "image", "json", "manifest", "object", "paintworklet",
        "report", "script", "serviceworker", "sharedworker", "style", "track",
        "video", "webidentity", "worker", "xslt"] }],
      CROSS_ORIGIN,
      ["referrerPolicy", "referrerpolicy", "enumerated", REFERRER_POLICY],
    ]],
    meta: ["HTMLMetaElement", [
      ["httpEquiv", "http-equiv"], "media", "scheme",
    ]],
    style: ["HTMLStyleElement", ["media"]],
    body: ["HTMLBodyElement", [
      ["link", "string", NULL_IS_EMPTY],
      ["vLink", "vlink", "string", NULL_IS_EMPTY],
      ["aLink", "alink", "string", NULL_IS_EMPTY],
      ["bgColor", "bgcolor", "string", NULL_IS_EMPTY],
      "background",
      ["text", "string", NULL_IS_EMPTY],
    ]],
    a: ["HTMLAnchorElement", [
      "target", "download", "ping",
      "rel", "hreflang", "charset",
      "rev", "shape", "coords",
      ["referrerPolicy", "referrerpolicy", "enumerated", REFERRER_POLICY],
    ]],
    area: ["HTMLAreaElement", [
      "coords", "download", "ping",
      "rel", "shape", "target",
      ["noHref", "nohref", "bool"], ["referrerPolicy", "referrerpolicy", "enumerated", REFERRER_POLICY],
      "alt", "hreflang", "type",
    ]],
    img: ["HTMLImageElement", [
      "srcset", "sizes", ["useMap", "usemap"],
      ["isMap", "ismap", "bool"], "align", "border",
      ["lowsrc", "url"], ["longDesc", "longdesc", "url"],
      ["width", "ulong"], ["height", "ulong"],
      ["hspace", "ulong"], ["vspace", "ulong"],
      ["decoding", "decoding", "enumerated",
        { keywords: ["sync", "async", "auto"], missing: "auto", invalid: "auto" }],
      LOADING,
      CROSS_ORIGIN, ["referrerPolicy", "referrerpolicy", "enumerated", REFERRER_POLICY],
    ]],
    embed: ["HTMLEmbedElement", [
      "width", "height", "align",
      "type", "name",
    ]],
    object: ["HTMLObjectElement", [
      ["data", "url"], ["useMap", "usemap"], "align",
      "type", "name",
      "archive", "code", ["declare", "bool"],
      "standby", ["codeBase", "codebase", "url"],
      ["codeType", "codetype"], "border",
      "width", "height",
      ["hspace", "ulong"], ["vspace", "ulong"],
    ]],
    param: ["HTMLParamElement", [
      ["valueType", "valuetype"], "name", "value", "type",
    ]],
    video: ["HTMLVideoElement", [
      ["poster", "url"], PRELOAD,
      LOADING,
      ["autoplay", "bool"], ["loop", "bool"],
      ["controls", "bool"], ["defaultMuted", "muted", "bool"],
      CROSS_ORIGIN,
      ["playsInline", "playsinline", "bool"],
      ["width", "ulong"], ["height", "ulong"],
    ]],
    audio: ["HTMLAudioElement", [
      PRELOAD,
      LOADING,
      ["autoplay", "bool"],
      ["loop", "bool"], ["controls", "bool"],
      ["defaultMuted", "muted", "bool"], CROSS_ORIGIN,
    ]],
    source: ["HTMLSourceElement", [
      "type",
      "srcset", "sizes", "media",
      ["width", "ulong"], ["height", "ulong"],
    ]],
    track: ["HTMLTrackElement", [
      "srclang", "label", ["default", "bool"],
      ["kind", "enumerated", {
        keywords: ["subtitles", "captions", "descriptions", "chapters", "metadata"],
        missing: "subtitles", invalid: "metadata" }],
    ]],
    map: ["HTMLMapElement", []],
    form: ["HTMLFormElement", [
      ["acceptCharset", "accept-charset"],
      ["action", "url", DOCUMENT_URL_WHEN_EMPTY],
      "autocomplete",
      ["enctype", "enumerated", ENCTYPE],
      ["encoding", "enctype", "enumerated", ENCTYPE],
      ["noValidate", "novalidate", "bool"], "target", "rel",
    ]],
    label: ["HTMLLabelElement", [["htmlFor", "for"]]],
    input: ["HTMLInputElement", [
      "accept", "autocomplete",
      ["defaultChecked", "checked", "bool"], ["dirName", "dirname"],
      ["formAction", "formaction", "url", DOCUMENT_URL_WHEN_EMPTY],
      ["formEnctype", "formenctype", "enumerated", FORM_ENCTYPE],
      ["formMethod", "formmethod", "enumerated", FORM_METHOD],
      ["formTarget", "formtarget"],
      ["formNoValidate", "formnovalidate", "bool"],
      "max", "min", "pattern",
      "placeholder", "step", ["useMap", "usemap"],
      "align", ["defaultValue", "value"],
      ["multiple", "bool"], ["required", "bool"],
      ["readOnly", "readonly", "bool"],
      ["maxLength", "maxlength", "long", { default: -1, nonNegative: true }],
      ["minLength", "minlength", "long", { default: -1, nonNegative: true }],
      ["size", "ulong", { default: 20, positive: true }],
      ["width", "ulong"], ["height", "ulong"],
    ]],
    button: ["HTMLButtonElement", [
      ["formAction", "formaction", "url", DOCUMENT_URL_WHEN_EMPTY],
      ["formEnctype", "formenctype", "enumerated", FORM_ENCTYPE],
      ["formMethod", "formmethod", "enumerated", FORM_METHOD],
      ["formTarget", "formtarget"],
      ["formNoValidate", "formnovalidate", "bool"],
    ]],
    select: ["HTMLSelectElement", [
      "autocomplete", ["multiple", "bool"],
      ["required", "bool"], ["size", "ulong"],
    ]],
    optgroup: ["HTMLOptGroupElement", ["label"]],
    option: ["HTMLOptionElement", [
      "label", ["defaultSelected", "selected", "bool"],
    ]],
    textarea: ["HTMLTextAreaElement", [
      "autocomplete", ["dirName", "dirname"],
      "placeholder", "wrap",
      ["required", "bool"], ["readOnly", "readonly", "bool"],
      ["maxLength", "maxlength", "long", { default: -1, nonNegative: true }],
      ["minLength", "minlength", "long", { default: -1, nonNegative: true }],
      ["cols", "ulong", { default: 20 }],
      ["rows", "ulong", { default: 2 }],
    ]],
    output: ["HTMLOutputElement", [["htmlFor", "for"]]],
    fieldset: ["HTMLFieldSetElement", []],
    legend: ["HTMLLegendElement", ["align"]],
    table: ["HTMLTableElement", [
      "align", "border", "frame",
      "rules", "summary", "width",
      ["bgColor", "bgcolor", "string", NULL_IS_EMPTY],
      ["cellPadding", "cellpadding", "string", NULL_IS_EMPTY],
      ["cellSpacing", "cellspacing", "string", NULL_IS_EMPTY],
    ]],
    caption: ["HTMLTableCaptionElement", ["align"]],
    col: ["HTMLTableColElement", [
      ["span", "ulong", { default: 1, clamp: [1, 1000] }], "align",
      ["ch", "char"], ["chOff", "charoff"], ["vAlign", "valign"],
      "width",
    ]],
    tr: ["HTMLTableRowElement", [
      "align", ["ch", "char"], ["chOff", "charoff"],
      ["vAlign", "valign"], ["bgColor", "bgcolor", "string", NULL_IS_EMPTY],
    ]],
    td: ["HTMLTableCellElement", [
      ["colSpan", "colspan", "ulong", { default: 1, clamp: [1, 1000] }],
      ["rowSpan", "rowspan", "ulong", { default: 1, clamp: [0, 65534] }],
      "headers", "abbr", "scope",
      "align", "axis", "height",
      "width", ["ch", "char"], ["chOff", "charoff"],
      ["noWrap", "nowrap", "bool"], ["vAlign", "valign"],
      ["bgColor", "bgcolor", "string", NULL_IS_EMPTY],
    ]],
    ol: ["HTMLOListElement", [
      ["reversed", "bool"], ["compact", "bool"],
      ["start", "long", { default: 1 }],
    ]],
    ul: ["HTMLUListElement", [["compact", "bool"]]],
    li: ["HTMLLIElement", [["value", "long"]]],
    dl: ["HTMLDListElement", [["compact", "bool"]]],
    blockquote: ["HTMLQuoteElement", [["cite", "url"]]],
    ins: ["HTMLModElement", [["cite", "url"], ["dateTime", "datetime"]]],
    script: ["HTMLScriptElement", [
      ["noModule", "nomodule", "bool"], ["async", "bool"],
      ["defer", "bool"], "integrity",
      "charset", "event", ["htmlFor", "for"],
      CROSS_ORIGIN, ["referrerPolicy", "referrerpolicy", "enumerated", REFERRER_POLICY],
    ]],
    marquee: ["HTMLMarqueeElement", [
      "behavior", ["bgColor", "bgcolor", "string", NULL_IS_EMPTY],
      "direction", "height", "width",
      ["hspace", "ulong"], ["vspace", "ulong"],
      ["trueSpeed", "truespeed", "bool"],
      ["scrollAmount", "scrollamount", "ulong", { default: 6 }],
      ["scrollDelay", "scrolldelay", "ulong", { default: 85 }],
      ["loop", "long", { default: -1 }],
    ]],
    applet: ["HTMLAppletElement", [
      "align", "archive", "code",
      ["codeBase", "codebase", "url"], "height",
      "object", "width",
      ["hspace", "ulong"], ["vspace", "ulong"],
    ]],
    frame: ["HTMLFrameElement", [
      "scrolling", ["frameBorder", "frameborder"],
      ["longDesc", "longdesc", "url"], ["noResize", "noresize", "bool"],
      ["marginHeight", "marginheight"], ["marginWidth", "marginwidth"],
    ]],
    frameset: ["HTMLFrameSetElement", ["cols", "rows"]],
    font: ["HTMLFontElement", [
      "color", "face", "size",
    ]],
    dir: ["HTMLDirectoryElement", [["compact", "bool"]]],
    hr: ["HTMLHRElement", [
      "align", "color", "size",
      "width", ["noShade", "noshade", "bool"],
    ]],
    pre: ["HTMLPreElement", [["width", "long"]]],
    details: ["HTMLDetailsElement", [["open", "bool"]]],
    dialog: ["HTMLDialogElement", [["open", "bool"]]],
    slot: ["HTMLSlotElement", []],
    canvas: ["HTMLCanvasElement", [
      ["width", "ulong", { default: 300 }],
      ["height", "ulong", { default: 150 }],
    ]],
    time: ["HTMLTimeElement", [["dateTime", "datetime"]]],
    data: ["HTMLDataElement", []],
    div: ["HTMLDivElement", ["align"]],
    h1: ["HTMLHeadingElement", ["align"]],
    // Interfaces the table simply did not have. Each is reflected in full by
    // `html/dom/reflection-*.html`, so a missing entry is not one attribute
    // missing — it is every attribute of that element failing at once.
    meter: ["HTMLMeterElement", [
      ["value", "double"], ["min", "double"],
      ["max", "double", { default: 1 }],
      ["low", "double"], ["high", "double"],
      ["optimum", "double"],
    ]],
    progress: ["HTMLProgressElement", [
      ["max", "double", { default: 1 }],
    ]],
    // `<iframe>` is not *loaded* here (§B6 refuses a second browsing context),
    // and its IDL reflection is a different question: an attribute that
    // reflects is testable and useful whether or not a document ever arrives
    // in the frame.
    iframe: ["HTMLIFrameElement", [
      ["src", "url"], "srcdoc", "name",
      "allow", "width", "height",
      "align", "scrolling",
      ["frameBorder", "frameborder"], ["longDesc", "longdesc", "url"],
      ["marginHeight", "marginheight"], ["marginWidth", "marginwidth"],
      ["allowFullscreen", "allowfullscreen", "bool"],
      LOADING,
      ["referrerPolicy", "referrerpolicy", "enumerated", REFERRER_POLICY],
    ]],
    del: ["HTMLModElement", [["cite", "url"], ["dateTime", "datetime"]]],
    q: ["HTMLQuoteElement", [["cite", "url"]]],
    th: ["HTMLTableCellElement", [
      ["colSpan", "colspan", "ulong", { default: 1, clamp: [1, 1000] }],
      ["rowSpan", "rowspan", "ulong", { default: 1, clamp: [0, 65534] }],
      "headers", "abbr", "scope",
      "align", "axis", "height",
      "width", ["ch", "char"], ["chOff", "charoff"],
      ["noWrap", "nowrap", "bool"], ["vAlign", "valign"],
      ["bgColor", "bgcolor", "string", NULL_IS_EMPTY],
    ]],
    thead: ["HTMLTableSectionElement", [
      "align", ["ch", "char"], ["chOff", "charoff"],
      ["vAlign", "valign"],
    ]],
    tfoot: ["HTMLTableSectionElement", [
      "align", ["ch", "char"], ["chOff", "charoff"],
      ["vAlign", "valign"],
    ]],
    colgroup: ["HTMLTableColElement", [
      ["span", "ulong", { default: 1, clamp: [1, 1000] }], "align",
      ["ch", "char"], ["chOff", "charoff"], ["vAlign", "valign"],
      "width",
    ]],
    tbody: ["HTMLTableSectionElement", [
      "align", ["ch", "char"], ["chOff", "charoff"],
      ["vAlign", "valign"],
    ]],
    p: ["HTMLParagraphElement", ["align"]],
    span: ["HTMLSpanElement", []],
    br: ["HTMLBRElement", ["clear"]],
    menu: ["HTMLMenuElement", [["compact", "bool"]]],
  };

  // Tags that share one interface with another tag, rather than repeating it.
  //
  // Only where the spec genuinely gives two tags one interface. <h1> is a
  // heading and <tbody> is a table section, so both got their own above rather
  // than being pointed at <p> and <tr>: `h1 instanceof HTMLParagraphElement`
  // would be false in every browser and true here, which is the kind of
  // almost-right this engine keeps having to remove.
  const SHARED = {
    colgroup: "col", th: "td", q: "blockquote", del: "ins",
    thead: "tbody", tfoot: "tbody",
    h2: "h1", h3: "h1", h4: "h1", h5: "h1", h6: "h1",
  };

  {
    const interfaces = {};
    for (const [tag, [name, attributes]] of Object.entries(REFLECTIONS)) {
      // One class per interface *name*, however many tags share it. Two
      // entries both declaring HTMLTableColElement used to mint two distinct
      // classes — elements constructed with one while the global was the
      // other, so `col instanceof HTMLTableColElement` was false for a col
      // whose constructor.name said otherwise. The union of the two entries'
      // attributes lands on the one prototype, which is also what the spec's
      // single interface holds.
      const Interface =
        interfaces[name] ?? { [name]: class extends Element {} }[name];
      for (const entry of attributes) {
        // A bare string is the common case and now says so: an IDL name that is
        // already its own content attribute name. 139 of this table's entries
        // were `["foo", "foo"]`, which is 1.3 KiB of the eagerly parsed prelude
        // spent writing each name twice — and the budget that guards that
        // parse is the reason it is worth spelling once.
        let [idl, content, type, options] =
          typeof entry === "string" ? [entry, entry] : entry;
        // `["foo", "bool"]` — a *type* in the content slot means the same
        // thing: the IDL name is its own content attribute name. No HTML
        // attribute is named after a reflection type, so the two slots cannot
        // be confused, and 55 more entries stop writing their name twice.
        if (REFLECT_TYPES.has(content)) {
          options = type; type = content; content = idl;
        }
        reflect(Interface.prototype, idl, content, type ?? "string", options ?? {});
      }
      // `Object.prototype.toString` on a `<p>` says `[object
      // HTMLParagraphElement]`, and the class string comes from here.
      Object.defineProperty(Interface.prototype, Symbol.toStringTag, {
        configurable: true,
        get() { return name; },
      });
      interfaces[name] = Interface;
      TAG_CLASSES.set(tag, Interface);
    }
    // <th> is a table cell and <del> is a mod: same interface, both tags.
    for (const [tag, like] of Object.entries(SHARED)) {
      if (TAG_CLASSES.has(like)) TAG_CLASSES.set(tag, TAG_CLASSES.get(like));
    }
    // `instanceof HTMLAnchorElement` is something pages genuinely write, and
    // an engine with the interfaces but no names for them fails it anyway.
    Object.assign(globalThis, interfaces);
  }

  // ── the properties that are not plain reflections ────────────────────────
  //
  // These nine sat on Element until WPT probed them, which meant
  // `"checked" in document.createElement("div")` was true and
  // `document.createElement("div").type` was `"text"`. That is the
  // `missingApi` lie at property scale: feature detection asks before it uses,
  // and every one of these is something code branches on.
  //
  // Each is installed on exactly the interfaces the spec gives it. The ones
  // that *are* plain reflections are listed here too, so the whole set of
  // "which tags have this" is in one place rather than split between two
  // mechanisms.
  {
    const on = (tags, name, descriptor) => {
      // Same brand rule as `reflect`: an accessor reached on the prototype
      // itself throws rather than dereferencing an `_id` that is not there.
      const guarded = { configurable: true, enumerable: true };
      if (descriptor.get) {
        guarded.get = function () {
          if (!this || this._id === undefined) {
            throw new TypeError(`Illegal invocation: ${name} needs an element`);
          }
          return descriptor.get.call(this);
        };
        Object.defineProperty(guarded.get, "name", { value: `get ${name}` });
      }
      if (descriptor.set) {
        guarded.set = function (value) {
          if (!this || this._id === undefined) {
            throw new TypeError(`Illegal invocation: ${name} needs an element`);
          }
          return descriptor.set.call(this, value);
        };
        Object.defineProperty(guarded.set, "name", { value: `set ${name}` });
      }
      if ("value" in descriptor) {
        guarded.value = descriptor.value;
        guarded.writable = descriptor.writable ?? true;
      }
      for (const tag of tags) {
        const Interface = TAG_CLASSES.get(tag);
        if (!Interface) continue;
        Object.defineProperty(Interface.prototype, name, guarded);
      }
    };
    const reflectOn = (tags, idl, content, type, options) => {
      for (const tag of tags) {
        const Interface = TAG_CLASSES.get(tag);
        if (Interface) reflect(Interface.prototype, idl, content, type ?? "string", options ?? {});
      }
    };

    // `href` and `src` are *resolved*, which is the difference between the
    // property and `getAttribute`. A page comparing `link.href` to
    // `location.href`, or reading `script.src` to find its own origin, gets the
    // absolute URL a browser would give it rather than the raw `../x` in the
    // markup.
    on(["a", "area", "link", "base"], "href", {
      get() { return this._resolved("href"); },
      set(v) { this.setAttribute("href", v); },
    });
    on(["img", "script", "embed", "source", "track", "audio", "video", "input"], "src", {
      get() { return this._resolved("src"); },
      set(v) { this.setAttribute("src", v); },
    });

    // The current value of a form control, which is not the `value` attribute:
    // a typed-in `<input>` and a `<select>` both answer from state, and
    // `<option>` falls back to its own text. `defaultValue` is the spec's name
    // for the attribute half and is reflected in the table above.
    on(["input", "option", "select", "textarea"], "value", {
      get() {
        const tag = this.tagName;
        if (tag === "SELECT") {
          const chosen = this.querySelectorAll("option").find((o) => o.selected);
          return chosen ? chosen.value : "";
        }
        if (tag === "OPTION") {
          const explicit = api.getAttr(this._id, "value");
          return explicit === null ? this.textContent : explicit;
        }
        // For `<input>` the *value mode* routes the read before any editor is
        // consulted: attribute-backed modes never see the dirty value, a file
        // input never has one, and every value-mode read passes through the
        // type's own sanitizer — which is why a `color` always answers seven
        // lowercase characters and a `number` never answers garbage.
        if (tag === "INPUT") {
          const kind = inputType(this);
          const mode = inputValueMode(kind);
          if (mode === "filename") return "";
          if (mode === "default") return api.getAttr(this._id, "value") ?? "";
          if (mode === "default/on") {
            const v = api.getAttr(this._id, "value");
            return v === null ? "on" : v;
          }
        }
        const raw = (() => {
          // The editor is the truth when there is one and it holds something:
          // typing updates it and leaves the `value` attribute at whatever the
          // HTML said.
          const edited = api.getValue(this._id);
          if (edited && edited.trim()) return edited;

          // A blank editor on a `<textarea>` is the case worth handling.
          if (tag === "TEXTAREA" && this._value === undefined) {
            const written = this.textContent;
            if (written) return written;
          }
          // **A whitespace-only editor on an unedited control is empty.** This is the same rule
          // the `<textarea>` branch above states, and it was being applied to textareas only —
          // so a laid-out `<input>` that nobody had typed into reported `" "`, because that is
          // what blitz seeds its editor with.
          if (edited !== null && edited !== undefined) {
            if (this._value === undefined && !edited.trim()) {
              return api.getAttr(this._id, "value") ?? "";
            }
            return edited;
          }
          // There is no editor — a detached control, or a `<textarea>`, which
          // blitz lays out as text rather than as an input. Falling back to
          // the markup is what a browser reports, and answering "" instead
          // made a filled-in comment box look empty to the agent reading it.
          if (this._value !== undefined) return this._value;
          if (tag === "TEXTAREA") return this.textContent;
          return api.getAttr(this._id, "value") ?? "";
        })();
        return tag === "INPUT" ? sanitizeInputValue(inputType(this), raw) : raw;
      },
      set(v) {
        const text = String(v);
        // `<option>` is not an editable control: its `value` reflects the
        // content attribute, falling back to the text. The generic path below
        // writes to the *editor*, which an option does not have, so the write
        // landed in `this._value` — where this element's getter never looks.
        // `option.value = "x"` was therefore silently lost, and with it
        // `new Option(label, value)`, which is most of why the constructor is
        // still written.
        if (this.tagName === "OPTION") {
          this.setAttribute("value", text);
          return;
        }
        // Assigning a select's value selects the first option that carries
        // it — and deselects everything when nothing does, which is how a
        // page discovers it assigned a value that is not on the menu.
        if (this.tagName === "SELECT") {
          let taken = false;
          for (const option of this.querySelectorAll("option")) {
            const hit = !taken && option.value === text;
            option._selected = hit;
            if (hit) taken = true;
          }
          return;
        }
        // The value modes again, on the write side: attribute-backed modes
        // write the attribute, a file input refuses anything but "", and a
        // value-mode write is sanitized on the way in with the caret moved to
        // the end — assigning `value` is the one write that parks the cursor
        // after the text.
        let stored = text;
        if (this.tagName === "INPUT") {
          const kind = inputType(this);
          const mode = inputValueMode(kind);
          if (mode === "filename") {
            if (text !== "") {
              throw new DOMException(
                "value: a file input's value can only be cleared",
                "InvalidStateError",
              );
            }
            delete this._value;
            return;
          }
          if (mode === "default" || mode === "default/on") {
            this.setAttribute("value", text);
            return;
          }
          stored = sanitizeInputValue(kind, text);
          if (["text", "search", "url", "tel", "password"].includes(kind)) {
            this.__h5iSelection =
              { start: stored.length, end: stored.length, direction: "none" };
          }
        } else if (this.tagName === "TEXTAREA") {
          this.__h5iSelection =
            { start: text.length, end: text.length, direction: "none" };
        }
        // Remembered on this side when the write had nowhere to land, so a
        // page that builds a control and fills it in can read back what it
        // wrote. A page that sets `.value` from script does not get
        // input/change: the spec fires those for *user* edits, and a framework
        // that re-rendered on its own write would loop. `Page::type_into` is
        // the user path.
        const landed = api.setValue(this._id, stored);
        if (!landed) {
          this._value = stored;
          if (this.tagName === "TEXTAREA") this.textContent = stored;
        } else {
          // The dirty flag must survive even when the editor took the write:
          // a later type change asks "was this control's value ever set by
          // script or typing", and the editor cannot answer that.
          this._value = stored;
        }
      },
    });

    // `checked` is state, not the attribute.
    for (const tag of ["button", "input"]) {
      on([tag], "popoverTargetElement", {
        get() {
          // The explicitly-assigned element wins over the attribute, but only
          // while it is actually in the document: a reference to a detached
          // element answers null, and comes back when the element is inserted.
          // That is the spec's "descendant of a shadow-including ancestor"
          // condition collapsed onto this engine's one flattened tree.
          const explicit = this.__h5iPopoverTarget;
          if (explicit !== undefined && explicit !== null) {
            return explicit.isConnected ? explicit : null;
          }
          const id = api.getAttr(this._id, "popovertarget");
          if (id === null || id === "") return null;
          return document.getElementById(id);
        },
        set(value) {
          // Assigning null clears both halves; assigning an element stores the
          // reference and stamps the attribute to "" — the attribute records
          // *that* a target is set, the reference records *which*, so an id
          // lookup never shadows the assignment.
          if (value === null || value === undefined) {
            this.__h5iPopoverTarget = null;
            this.removeAttribute("popovertarget");
            return;
          }
          if (value._id === undefined) {
            throw new TypeError("popoverTargetElement must be an Element or null");
          }
          this.__h5iPopoverTarget = value;
          this.setAttribute("popovertarget", "");
        },
      });
      on([tag], "popoverTargetAction", {
        get() {
          const raw = (api.getAttr(this._id, "popovertargetaction") || "").toLowerCase();
          return raw === "show" || raw === "hide" ? raw : "toggle";
        },
        set(value) { this.setAttribute("popovertargetaction", String(value)); },
      });
    }

    // The Invoker Commands pair, `popovertarget` generalised to any element
    // and an explicit verb. `commandForElement` follows the same
    // reflected-element rules as `popoverTargetElement` above — explicit
    // reference wins while connected, otherwise the attribute's id resolves.
    on(["button"], "commandForElement", {
      get() {
        const explicit = this.__h5iCommandFor;
        if (explicit !== undefined && explicit !== null) {
          return explicit.isConnected ? explicit : null;
        }
        const id = api.getAttr(this._id, "commandfor");
        if (id === null || id === "") return null;
        return document.getElementById(id);
      },
      set(value) {
        if (value === null || value === undefined) {
          this.__h5iCommandFor = null;
          this.removeAttribute("commandfor");
          return;
        }
        if (value._id === undefined) {
          throw new TypeError("commandForElement must be an Element or null");
        }
        this.__h5iCommandFor = value;
        this.setAttribute("commandfor", "");
      },
    });
    // `command` is not a plain reflection: unknown verbs read back as "", and
    // a page-defined `--verb` reads back exactly as written — that prefix is
    // the namespace the built-ins can never grow into.
    const KNOWN_COMMANDS = [
      "toggle-popover", "show-popover", "hide-popover",
      "close", "request-close", "show-modal",
    ];
    on(["button"], "command", {
      get() {
        const raw = api.getAttr(this._id, "command");
        if (raw === null) return "";
        if (raw.startsWith("--")) return raw;
        const low = raw.toLowerCase();
        return KNOWN_COMMANDS.includes(low) ? low : "";
      },
      set(value) { this.setAttribute("command", String(value)); },
    });

    // ---- media state -------------------------------------------------------
    //
    // This engine does not play media, and the surface below says so honestly
    // rather than pretending: nothing is ever playing (`paused` true, `ended`
    // false, `currentTime` wherever the page last put it), no data ever
    // arrives (`readyState` HAVE_NOTHING, empty `buffered`), and `play()`
    // rejects with the NotSupportedError a browser uses for a source it
    // cannot decode. A page that branches on these gets the no-media branch,
    // which is the true one.
    {
      const emptyTimeRanges = () => ({
        length: 0,
        start() { throw new DOMException("no ranges", "IndexSizeError"); },
        end() { throw new DOMException("no ranges", "IndexSizeError"); },
      });
      const media = ["audio", "video"];
      on(media, "paused", { get() { return true; } });
      on(media, "ended", { get() { return false; } });
      on(media, "seeking", { get() { return false; } });
      on(media, "duration", { get() { return NaN; } });
      on(media, "networkState", { get() { return 0; } });
      on(media, "readyState", { get() { return 0; } });
      // A real MediaError, and honestly earned: this engine decodes nothing,
      // so any media element *with a source* is an element whose source is
      // not supported — code 4, the same answer a browser gives for a codec
      // it lacks. No source, no error, exactly as the spec has it.
      on(media, "error", {
        get() {
          const src = api.getAttr(this._id, "src");
          const hasSource = (src !== null && src !== "")
            || this.querySelector("source") !== null;
          if (!hasSource) return null;
          const error = Object.create(MediaError.prototype);
          Object.defineProperty(error, "__h5iCode", { value: 4 });
          return error;
        },
      });
      on(media, "currentSrc", { get() { return this._resolved("src"); } });
      on(media, "buffered", { get() { return emptyTimeRanges(); } });
      on(media, "played", { get() { return emptyTimeRanges(); } });
      on(media, "seekable", { get() { return emptyTimeRanges(); } });
      on(media, "currentTime", {
        get() { return this.__h5iMediaTime ?? 0; },
        set(value) { this.__h5iMediaTime = Number(value) || 0; },
      });
      on(media, "playbackRate", {
        get() { return this.__h5iMediaRate ?? 1; },
        set(value) { this.__h5iMediaRate = Number(value); },
      });
      on(media, "defaultPlaybackRate", {
        get() { return this.__h5iMediaDefaultRate ?? 1; },
        set(value) { this.__h5iMediaDefaultRate = Number(value); },
      });
      on(media, "volume", {
        get() { return this.__h5iMediaVolume ?? 1; },
        set(value) { this.__h5iMediaVolume = Number(value); },
      });
      on(media, "muted", {
        get() { return this.__h5iMediaMuted ?? (api.getAttr(this._id, "muted") !== null); },
        set(value) { this.__h5iMediaMuted = !!value; },
      });
      on(media, "play", {
        value() {
          return Promise.reject(
            new DOMException("play: this engine does not decode media", "NotSupportedError"),
          );
        },
      });
      on(media, "pause", { value() {} });
      on(media, "load", { value() {} });
      on(media, "canPlayType", { value() { return ""; } });
      on(["video"], "videoWidth", { get() { return 0; } });
      on(["video"], "videoHeight", { get() { return 0; } });
    }

    // Small read sides pages and idlharness both reach for.
    on(["select"], "selectedOptions", {
      get() {
        return collection(
          Array.from(this.querySelectorAll("option")).filter((o) => o.selected),
          "HTMLCollection",
        );
      },
    });
    on(["select"], "type", {
      get() { return api.getAttr(this._id, "multiple") !== null ? "select-multiple" : "select-one"; },
    });
    on(["select"], "selectedIndex", {
      get() {
        const options = Array.from(this.querySelectorAll("option"));
        const chosen = selectedOptionsOf(this)[0];
        if (!chosen) return -1;
        return options.findIndex((o) => o._id === chosen._id);
      },
      set(index) {
        const options = Array.from(this.querySelectorAll("option"));
        const at = Number(index);
        for (let i = 0; i < options.length; i += 1) {
          options[i]._selected = i === at;
        }
      },
    });
    on(["textarea"], "type", { get() { return "textarea"; } });
    on(["textarea"], "textLength", { get() { return this.value.length; } });
    on(["output"], "type", { get() { return "output"; } });
    on(["fieldset"], "type", { get() { return "fieldset"; } });
    on(["fieldset"], "elements", {
      get() {
        return collection(
          Array.from(this.querySelectorAll("input,button,select,textarea,output,object,fieldset")),
          "HTMLCollection",
        );
      },
    });
    // An image that never loaded has no natural size, and `complete` is true
    // for the no-src case exactly as the spec says.
    on(["img"], "naturalWidth", { get() { return this.width; } });
    on(["img"], "naturalHeight", { get() { return this.height; } });
    // True always: by the time script runs, any load this engine was going to
    // do has happened — success or failure, both of which are "complete".
    on(["img"], "complete", { get() { return true; } });
    // The srcset microsyntax, parsed as the spec parses it: spec whitespace
    // is exactly TAB/LF/FF/CR/SPACE (a NBSP is part of the URL), a trailing
    // comma ends a candidate but an embedded one is a split, parentheses
    // swallow commas inside a descriptor, and a candidate with any invalid
    // descriptor is dropped whole. WPT walks every one of those edges.
    function parseSrcset(input) {
      const ws = (c) => c === "\t" || c === "\n" || c === "\f" || c === "\r" || c === " ";
      const candidates = [];
      let pos = 0;
      const len = input.length;
      while (pos < len) {
        while (pos < len && (ws(input[pos]) || input[pos] === ",")) pos += 1;
        if (pos >= len) break;
        const start = pos;
        while (pos < len && !ws(input[pos])) pos += 1;
        let url = input.slice(start, pos);
        const descriptors = [];
        if (url.endsWith(",")) {
          url = url.replace(/,+$/, "");
          if (url === "") continue;
        } else {
          while (pos < len && ws(input[pos])) pos += 1;
          let current = "";
          let inParens = false;
          let splitting = true;
          while (pos < len && splitting) {
            const c = input[pos];
            if (inParens) {
              if (c === ")") inParens = false;
              current += c;
              pos += 1;
            } else if (c === ",") {
              pos += 1;
              splitting = false;
            } else if (c === "(") {
              inParens = true;
              current += c;
              pos += 1;
            } else if (ws(c)) {
              if (current) { descriptors.push(current); current = ""; }
              pos += 1;
            } else {
              current += c;
              pos += 1;
            }
          }
          if (current) descriptors.push(current);
        }
        let width = null;
        let density = null;
        let height = null;
        let valid = true;
        for (const desc of descriptors) {
          const unit = desc[desc.length - 1];
          const number = desc.slice(0, -1);
          const isInt = /^\d+$/.test(number);
          const isFloat = /^-?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$/.test(number);
          if (unit === "w" && isInt && Number(number) > 0
            && width === null && density === null) {
            width = Number(number);
          } else if (unit === "x" && isFloat && Number(number) >= 0
            && width === null && density === null && height === null) {
            density = Number(number);
          } else if (unit === "h" && isInt && Number(number) > 0
            && height === null && density === null) {
            height = Number(number);
          } else {
            valid = false;
            break;
          }
        }
        if (height !== null && width === null) valid = false;
        if (valid) candidates.push({ url, width, density });
      }
      return candidates;
    }
    // What the engine chose to load: the first valid srcset candidate — this
    // engine renders at 1x on one viewport, so the elaborate density and
    // width selection collapses to document order — else the plain src,
    // else "".
    on(["img"], "currentSrc", {
      get() {
        const srcset = api.getAttr(this._id, "srcset");
        if (srcset !== null && srcset !== "") {
          const candidates = parseSrcset(srcset);
          if (candidates.length > 0) {
            const chosen = candidates[0].url;
            const parts = api.parseUrl(chosen, currentAddress);
            return parts ? parts.href : chosen;
          }
        }
        const src = api.getAttr(this._id, "src");
        if (src === null || src === "") return "";
        return this._resolved("src");
      },
    });
    on(["a"], "text", {
      get() { return this.textContent; },
      set(value) { this.textContent = String(value); },
    });
    on(["title"], "text", {
      get() { return this.textContent; },
      set(value) { this.textContent = String(value); },
    });
    on(["script"], "text", {
      get() { return this.textContent; },
      set(value) { this.textContent = String(value); },
    });
    on(["map"], "areas", {
      get() { return collection(Array.from(this.querySelectorAll("area")), "HTMLCollection"); },
    });
    on(["label"], "control", {
      get() {
        const forId = api.getAttr(this._id, "for");
        if (forId !== null) {
          return forId === "" ? null : document.getElementById(forId);
        }
        return this.querySelector("input,button,select,textarea,output,meter,progress") ?? null;
      },
    });
    on(["progress"], "position", {
      get() {
        const max = Number(api.getAttr(this._id, "max")) || 1;
        const raw = api.getAttr(this._id, "value");
        if (raw === null) return -1;
        const value = Number(raw) || 0;
        return Math.min(Math.max(value / max, 0), 1);
      },
    });

    // ---- `<dialog>` --------------------------------------------------------
    //
    // `open` reflected and nothing could change it: `show()`, `showModal()`
    // and `close()` were all absent, so a dialog could be described and never
    // opened. As with popovers the state machine and the events are real and
    // the *modality* is not — there is no top layer and no inert subtree, so a
    // modal dialog here does not block the page behind it. That difference is
    // about rendering and focus, and the API contract a page scripts against
    // is not.
    on(["dialog"], "returnValue", {
      get() { return this.__h5iReturnValue ?? ""; },
      set(value) { this.__h5iReturnValue = String(value); },
    });
    for (const [name, modal] of [["show", false], ["showModal", true]]) {
      Object.defineProperty(Element.prototype, name, {
        configurable: true,
        writable: true,
        value: function () {
          if (this.tagName !== "DIALOG") {
            throw new TypeError(`${name} is only defined on <dialog>`);
          }
          if (this.hasAttribute("open")) {
            // Re-showing an already-open dialog is a no-op for `show()` and an
            // error for `showModal()`, which is the one asymmetry here.
            if (!modal) return;
            throw new DOMException(
              "showModal: the dialog is already open",
              "InvalidStateError",
            );
          }
          if (modal && !this.isConnected) {
            throw new DOMException(
              "showModal: the dialog is not connected",
              "InvalidStateError",
            );
          }
          this.setAttribute("open", "");
          this.__h5iModal = modal;
          if (modal) this.classList.add(MODAL_OPEN_CLASS);
          // The dialog focusing steps: an `autofocus` descendant wins, then
          // the first focusable control, then the dialog itself — which is
          // how `document.activeElement` knows a dialog opened.
          const preferred = this.querySelector("[autofocus]")
            ?? this.querySelector("input, button, select, textarea, a[href], [tabindex]")
            ?? this;
          if (typeof preferred.focus === "function") preferred.focus();
        },
      });
    }
    Object.defineProperty(Element.prototype, "close", {
      configurable: true,
      writable: true,
      value: function (returnValue) {
        if (this.tagName !== "DIALOG") {
          throw new TypeError("close is only defined on <dialog>");
        }
        if (!this.hasAttribute("open")) return;
        if (returnValue !== undefined) this.__h5iReturnValue = String(returnValue);
        this.removeAttribute("open");
        this.__h5iModal = false;
        this.classList.remove(MODAL_OPEN_CLASS);
        // `close` does not bubble, which is what a page delegating from an
        // ancestor has to know and what makes this worth getting right.
        this.dispatchEvent(new Event("close"));
      },
    });
    // The polite `close`: asks first. `cancel` is the asking — a listener that
    // prevents it keeps the dialog open, which is exactly what pressing
    // Escape runs through in a browser.
    Object.defineProperty(Element.prototype, "requestClose", {
      configurable: true,
      writable: true,
      value: function (returnValue) {
        if (this.tagName !== "DIALOG") {
          throw new TypeError("requestClose is only defined on <dialog>");
        }
        if (!this.hasAttribute("open")) return;
        const cancel = new Event("cancel", { cancelable: true });
        this.dispatchEvent(cancel);
        if (cancel.defaultPrevented) return;
        this.close(returnValue);
      },
    });

    // `showPicker` opens nothing here — there is no picker to draw — but the
    // *guards* are the API's contract and are what WPT exercises: disabled
    // and readonly controls refuse with InvalidStateError, and without a user
    // gesture nothing opens anywhere, which is NotAllowedError. A permitted
    // call spends the activation, exactly as a real picker would.
    on(["input", "select"], "showPicker", {
      value() {
        if (this.disabled) {
          throw new DOMException("showPicker: the control is disabled", "InvalidStateError");
        }
        if (this.tagName === "INPUT") {
          const kind = inputType(this);
          const readonlyApplies = ![
            "button", "checkbox", "color", "file", "hidden", "image", "radio",
            "range", "reset", "submit",
          ].includes(kind);
          if (readonlyApplies && api.getAttr(this._id, "readonly") !== null) {
            throw new DOMException("showPicker: the control is readonly", "InvalidStateError");
          }
        }
        if (!userActivation.active) {
          throw new DOMException("showPicker: needs a user gesture", "NotAllowedError");
        }
        userActivation.active = false;
      },
    });

    // ---- ElementInternals --------------------------------------------------
    //
    // The half of the custom-elements contract that lives *behind* the
    // element: default ARIA that never touches the host's attributes, and
    // form participation for `formAssociated` classes. The ARIA state here is
    // storage with the right names — this engine computes no accessibility
    // tree — and the validity half is real: `setValidity` feeds the same
    // answers a built-in control's constraint validation gives.
    {
      class ElementInternals {
        constructor() { throw new TypeError("Illegal constructor"); }
        setValidity(flags = {}, message, anchor) {
          const any = Object.keys(flags).some((k) => flags[k]);
          if (any && !message) {
            throw new TypeError(
              "setValidity: a message is required when any flag is set",
            );
          }
          this.__h5iValidity = { ...flags };
          this.__h5iValidationMessage = any ? String(message) : "";
          void anchor;
        }
        checkValidity() { return this.validity.valid; }
        reportValidity() { return this.checkValidity(); }
        setFormValue(value, state) {
          this.__h5iRequireFormAssociated("setFormValue");
          this.__h5iFormValue = value;
          void state;
        }
        __h5iRequireFormAssociated(op) {
          const host = this.__h5iHost;
          if (!host || !host.constructor || host.constructor.formAssociated !== true) {
            throw new DOMException(
              `${op}: the element is not form-associated`,
              "NotSupportedError",
            );
          }
        }
        get shadowRoot() { return this.__h5iHost.shadowRoot ?? null; }
        get form() {
          this.__h5iRequireFormAssociated("form");
          return this.__h5iHost.form ?? null;
        }
        get willValidate() {
          this.__h5iRequireFormAssociated("willValidate");
          return true;
        }
        get validity() {
          const flags = this.__h5iValidity ?? {};
          const valid = !Object.keys(flags).some((k) => flags[k]);
          return Object.freeze({
            valueMissing: false, typeMismatch: false, patternMismatch: false,
            tooLong: false, tooShort: false, rangeUnderflow: false,
            rangeOverflow: false, stepMismatch: false, badInput: false,
            customError: false,
            ...flags, valid,
          });
        }
        get validationMessage() { return this.__h5iValidationMessage ?? ""; }
        get labels() {
          this.__h5iRequireFormAssociated("labels");
          return this.__h5iHost.labels ?? collection([], "NodeList");
        }
        get states() {
          if (!this.__h5iStates) this.__h5iStates = new Set();
          return this.__h5iStates;
        }
      }
      // The ARIA mixin, as internal state with the IDL names: null until set,
      // never reflected into the host's markup — that separation is the
      // feature.
      for (const name of [
        "role", "ariaAtomic", "ariaAutoComplete", "ariaBrailleLabel",
        "ariaBrailleRoleDescription", "ariaBusy", "ariaChecked", "ariaColCount",
        "ariaColIndex", "ariaColIndexText", "ariaColSpan", "ariaCurrent",
        "ariaDescription", "ariaDisabled", "ariaExpanded", "ariaHasPopup",
        "ariaHidden", "ariaInvalid", "ariaKeyShortcuts", "ariaLabel",
        "ariaLevel", "ariaLive", "ariaModal", "ariaMultiLine",
        "ariaMultiSelectable", "ariaOrientation", "ariaPlaceholder",
        "ariaPosInSet", "ariaPressed", "ariaReadOnly", "ariaRelevant",
        "ariaRequired", "ariaRoleDescription", "ariaRowCount", "ariaRowIndex",
        "ariaRowIndexText", "ariaRowSpan", "ariaSelected", "ariaSetSize",
        "ariaSort", "ariaValueMax", "ariaValueMin", "ariaValueNow",
        "ariaValueText",
      ]) {
        const slot = `__h5iAria_${name}`;
        const getter = function () { return this[slot] ?? null; };
        const setter = function (value) {
          this[slot] = value === null || value === undefined ? null : String(value);
        };
        Object.defineProperty(getter, "name", { value: `get ${name}` });
        Object.defineProperty(setter, "name", { value: `set ${name}` });
        Object.defineProperty(ElementInternals.prototype, name, {
          configurable: true, enumerable: true, get: getter, set: setter,
        });
      }
      Object.defineProperty(ElementInternals.prototype, Symbol.toStringTag, {
        value: "ElementInternals", configurable: true,
      });
      globalThis.ElementInternals = ElementInternals;

      Object.defineProperty(Element.prototype, "attachInternals", {
        configurable: true, writable: true,
        value: function attachInternals() {
          if (!this || this._id === undefined) {
            throw new TypeError("Illegal invocation: attachInternals needs an element");
          }
          // Only an autonomous custom element has internals to attach, and
          // only once — both refusals are the spec's.
          if (!String(this.tagName).includes("-")) {
            throw new DOMException(
              "attachInternals: not a custom element",
              "NotSupportedError",
            );
          }
          if (this.__h5iInternals) {
            throw new DOMException(
              "attachInternals: already attached",
              "NotSupportedError",
            );
          }
          const internals = Object.create(ElementInternals.prototype);
          Object.defineProperty(internals, "__h5iHost", { value: this });
          Object.defineProperty(this, "__h5iInternals", { value: internals });
          return internals;
        },
      });
    }

    // `indeterminate` is pure state — never reflected, cleared by a user
    // click, drawn as the dash a page uses for "some but not all selected".
    on(["input"], "indeterminate", {
      get() { return !!this.__h5iIndeterminate; },
      set(value) { this.__h5iIndeterminate = !!value; },
    });
    // The `<datalist>` the `list` attribute points at — the element, not the
    // id, and only when it really is a datalist.
    on(["input"], "list", {
      get() {
        const id = api.getAttr(this._id, "list");
        if (id === null || id === "") return null;
        const el = document.getElementById(id);
        return el && el.tagName === "DATALIST" ? el : null;
      },
    });
    // The labels pointing at a control: `<label for>` by id, or the label an
    // element sits inside. A hidden input answers null — it is not labelable.
    on(["input", "button", "select", "textarea", "output", "meter", "progress"],
      "labels", {
        get() {
          if (this.tagName === "INPUT" && this.type === "hidden") return null;
          const id = this.id;
          const out = [];
          for (const label of document.querySelectorAll("label")) {
            const forId = api.getAttr(label._id, "for");
            if (forId !== null ? (id !== "" && forId === id) : label.contains(this)) {
              out.push(label);
            }
          }
          return collection(out, "NodeList");
        },
      });

    on(["input"], "checked", {
      get() {
        if (this._checked !== undefined) return this._checked;
        return api.getAttr(this._id, "checked") !== null;
      },
      set(on_) { this._checked = !!on_; },
    });
    // Selectedness is *state*: the attribute is only the default. In a
    // single select the last default-selected option wins, and when nothing
    // is selected the first non-disabled option is — that fallback is why a
    // dropdown always shows something.
    function selectAncestorOf(option) {
      for (let n = option.parentNode; n; n = n.parentNode) {
        if (n.tagName === "SELECT") return n;
      }
      return null;
    }
    function selectedOptionsOf(sel) {
      const options = Array.from(sel.querySelectorAll("option"));
      const multiple = api.getAttr(sel._id, "multiple") !== null;
      const isOn = (o) => o._selected !== undefined
        ? o._selected
        : api.getAttr(o._id, "selected") !== null;
      const explicit = options.filter(isOn);
      if (multiple) return explicit;
      if (explicit.length > 0) return [explicit[explicit.length - 1]];
      const size = Number(api.getAttr(sel._id, "size")) || 1;
      if (size <= 1) {
        const first = options.find((o) => !o.disabled);
        return first ? [first] : [];
      }
      return [];
    }
    on(["option"], "selected", {
      get() {
        if (this._selected !== undefined) return this._selected;
        const sel = selectAncestorOf(this);
        if (!sel) return api.getAttr(this._id, "selected") !== null;
        return selectedOptionsOf(sel).some((o) => o._id === this._id);
      },
      set(on_) {
        // State, not markup: `defaultSelected` is the attribute's reflection
        // and stays untouched. Selecting in a single select deselects peers.
        const want = !!on_;
        const sel = selectAncestorOf(this);
        if (want && sel && api.getAttr(sel._id, "multiple") === null) {
          for (const other of sel.querySelectorAll("option")) {
            if (other._id !== this._id) other._selected = false;
          }
        }
        this._selected = want;
      },
    });

    // `<input>` is the one element whose missing `type` is not the empty
    // string: an input with no type attribute is a text input, and code reads
    // `input.type` to decide how to treat it.
    const KNOWN_INPUT_TYPES = new Set([
      "hidden", "text", "search", "tel", "url", "email", "password", "date",
      "month", "week", "time", "datetime-local", "number", "range", "color",
      "checkbox", "radio", "file", "submit", "image", "reset", "button",
    ]);
    function inputType(el) {
      const raw = (api.getAttr(el._id, "type") || "").toLowerCase();
      return KNOWN_INPUT_TYPES.has(raw) ? raw : "text";
    }
    // Which of the spec's four *value modes* a type is in — the axis that
    // decides where `value` lives: the dirty value, the attribute, "on", or a
    // filename this engine will never have.
    function inputValueMode(type) {
      if (type === "file") return "filename";
      if (type === "checkbox" || type === "radio") return "default/on";
      if (["hidden", "submit", "image", "reset", "button"].includes(type)) {
        return "default";
      }
      return "value";
    }
    // The per-type *value sanitization algorithm*. Every value-mode-value type
    // has one, and the tests change type mid-flight precisely to watch the
    // new type's rules bite the old type's value.
    function sanitizeInputValue(type, raw) {
      const value = String(raw);
      const isFiniteNumber = (s) => s !== "" && /^-?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$/.test(s);
      const validDate = (s) => {
        const m = /^(\d{4,})-(\d{2})-(\d{2})$/.exec(s);
        if (!m) return false;
        const [, y, mo, d] = m.map(Number);
        if (y < 1 || mo < 1 || mo > 12) return false;
        const days = new Date(y, mo, 0).getDate();
        return d >= 1 && d <= days;
      };
      const validTime = (s) => /^([01]\d|2[0-3]):[0-5]\d(:[0-5]\d(\.\d{1,3})?)?$/.test(s);
      switch (type) {
        case "text": case "search": case "tel": case "password":
          return value.replace(/[\r\n]/g, "");
        case "url": case "email":
          return value.replace(/[\r\n]/g, "").trim();
        case "number":
          return isFiniteNumber(value) ? value : "";
        case "range": {
          if (isFiniteNumber(value)) return value;
          // The default of a range is the middle of its track.
          return "50";
        }
        case "color":
          return /^#[0-9a-fA-F]{6}$/.test(value) ? value.toLowerCase() : "#000000";
        case "date":
          return validDate(value) ? value : "";
        case "time":
          return validTime(value) ? value : "";
        case "month":
          return /^(\d{4,})-(0[1-9]|1[0-2])$/.test(value) ? value : "";
        case "week":
          return /^(\d{4,})-W(0[1-9]|[1-4]\d|5[0-3])$/.test(value) ? value : "";
        case "datetime-local": {
          const parts = value.split("T");
          return parts.length === 2 && validDate(parts[0]) && validTime(parts[1])
            ? value : "";
        }
        default:
          return value;
      }
    }
    on(["input"], "type", {
      get() { return inputType(this); },
      set(v) {
        // A type change is a *state* change, and the spec walks the value
        // between modes: a dirty value entering an attribute-backed mode is
        // written into the attribute, a value entering filename mode is gone,
        // and whatever arrives in a value-mode type meets that type's
        // sanitizer. Selection resets to the start when the control becomes
        // selectable.
        const oldType = inputType(this);
        const wasSelectable =
          this.tagName === "INPUT" && ["text", "search", "url", "tel", "password"].includes(oldType);
        const oldMode = inputValueMode(oldType);
        const dirty = this._value !== undefined;
        const oldDirty = dirty ? this._value : null;
        this.setAttribute("type", v);
        const newType = inputType(this);
        const newMode = inputValueMode(newType);
        if (oldMode === "value" && dirty
          && (newMode === "default" || newMode === "default/on")) {
          api.setAttr(this._id, "value", oldDirty);
          delete this._value;
        } else if (newMode === "filename") {
          delete this._value;
        } else if (newMode === "value" && dirty) {
          this._value = sanitizeInputValue(newType, oldDirty);
        }
        const nowSelectable = ["text", "search", "url", "tel", "password"].includes(newType);
        if (!wasSelectable && nowSelectable) {
          this.__h5iSelection = { start: 0, end: 0, direction: "none" };
        }
      },
    });
    reflectOn(["a", "link", "script", "style", "embed", "object", "source",
               "param", "ol", "ul", "li", "button"], "type", "type");

    reflectOn(["img", "area", "input", "applet"], "alt", "alt");
    reflectOn(["input", "button", "select", "optgroup", "option", "textarea",
               "fieldset", "link", "style"], "disabled", "disabled", "bool");
    reflectOn(["form", "input", "select", "textarea", "button", "output",
               "fieldset", "object", "param", "map", "meta", "a", "img",
               "embed", "frame", "applet", "slot"], "name", "name");
    reflectOn(["button", "param", "data"], "value", "value");

    // Canvas 2D, drawn for real.
    class CanvasRenderingContext2D {
      constructor(canvas) {
        this.canvas = canvas;
        this._node = canvas._id;
        // Mirrored on the JS side because the spec requires reading them back,
        // and a getter that asked Rust for each would be a round trip per
        // property read in a draw loop.
        this._fillStyle = "#000000";
        this._strokeStyle = "#000000";
        this._lineWidth = 1;
        this._globalAlpha = 1;
        this._lineCap = "butt";
        this._lineJoin = "miter";
      }

      // Every drawing call, under one door. `false` back means the engine does
      // not have this one, and saying so is the whole point.
      _op(name, args) {
        if (api.canvasOp(this._node, name, args || []) === false) {
          api.unsupported(`CanvasRenderingContext2D.${name}`);
          return false;
        }
        return true;
      }

      get fillStyle() { return this._fillStyle; }
      set fillStyle(value) {
        // A colour this engine cannot parse is *reported*, and the previous
        // one stands — which is the spec's rule for an invalid value and also
        // keeps a gradient object from being read as if it were a colour.
        if (this._op("fillStyle", [String(value)])) this._fillStyle = String(value);
      }

      get strokeStyle() { return this._strokeStyle; }
      set strokeStyle(value) {
        if (this._op("strokeStyle", [String(value)])) this._strokeStyle = String(value);
      }

      get lineWidth() { return this._lineWidth; }
      set lineWidth(value) {
        this._lineWidth = Number(value);
        this._op("lineWidth", [Number(value)]);
      }

      get globalAlpha() { return this._globalAlpha; }
      set globalAlpha(value) {
        this._globalAlpha = Number(value);
        this._op("globalAlpha", [Number(value)]);
      }

      get lineCap() { return this._lineCap; }
      set lineCap(value) {
        this._lineCap = String(value);
        this._op("lineCap", [String(value)]);
      }

      get lineJoin() { return this._lineJoin; }
      set lineJoin(value) {
        this._lineJoin = String(value);
        this._op("lineJoin", [String(value)]);
      }

      save() { this._op("save", []); }
      restore() { this._op("restore", []); }

      translate(x, y) { this._op("translate", [+x, +y]); }
      scale(x, y) { this._op("scale", [+x, +y]); }
      rotate(a) { this._op("rotate", [+a]); }
      transform(a, b, c, d, e, f) { this._op("transform", [+a, +b, +c, +d, +e, +f]); }
      setTransform(a, b, c, d, e, f) { this._op("setTransform", [+a, +b, +c, +d, +e, +f]); }
      resetTransform() { this._op("resetTransform", []); }

      beginPath() { this._op("beginPath", []); }
      closePath() { this._op("closePath", []); }
      moveTo(x, y) { this._op("moveTo", [+x, +y]); }
      lineTo(x, y) { this._op("lineTo", [+x, +y]); }
      quadraticCurveTo(cx, cy, x, y) { this._op("quadraticCurveTo", [+cx, +cy, +x, +y]); }
      bezierCurveTo(a, b, c, d, e, f) { this._op("bezierCurveTo", [+a, +b, +c, +d, +e, +f]); }
      rect(x, y, w, h) { this._op("rect", [+x, +y, +w, +h]); }
      arc(x, y, r, s, e, ccw) { this._op("arc", [+x, +y, +r, +s, +e, ccw ? 1 : 0]); }

      fill(rule) { this._op("fill", rule ? [String(rule)] : []); }
      stroke() { this._op("stroke", []); }
      fillRect(x, y, w, h) { this._op("fillRect", [+x, +y, +w, +h]); }
      strokeRect(x, y, w, h) { this._op("strokeRect", [+x, +y, +w, +h]); }
      clearRect(x, y, w, h) { this._op("clearRect", [+x, +y, +w, +h]); }
    }

    // The operations this engine does not have, present and reporting.
    for (const name of [
      "fillText", "strokeText", "drawImage", "clip", "setLineDash",
      "ellipse", "arcTo", "roundRect", "putImageData", "createImageData",
    ]) {
      CanvasRenderingContext2D.prototype[name] = function (...args) {
        this._op(name, args.filter((a) => typeof a === "number"));
      };
    }

    // The value-returning half. Reported the same way, and answering `null`
    // rather than a plausible number: a `measureText` that claims a width this
    // engine never measured is the wrong-answer-that-looks-right this whole
    // engine is built to refuse, and a page that lays out against it would be
    // laying out against fiction.
    for (const name of [
      "measureText", "createLinearGradient", "createRadialGradient",
      "createConicGradient", "createPattern", "getImageData", "getLineDash",
      "isPointInPath", "isPointInStroke",
    ]) {
      CanvasRenderingContext2D.prototype[name] = function () {
        api.unsupported(`CanvasRenderingContext2D.${name}`);
        return null;
      };
    }

    // Properties that configure something unbuilt. Settable, so assignment
    // does not throw and the surrounding code runs, and named on the way past.
    for (const name of [
      "font", "textAlign", "textBaseline", "shadowBlur", "shadowColor",
      "shadowOffsetX", "shadowOffsetY", "globalCompositeOperation",
      "imageSmoothingEnabled", "filter", "miterLimit", "direction",
    ]) {
      Object.defineProperty(CanvasRenderingContext2D.prototype, name, {
        configurable: true,
        get() { return this[`_${name}`]; },
        set(value) {
          this[`_${name}`] = value;
          api.unsupported(`CanvasRenderingContext2D.${name}`);
        },
      });
    }
    globalThis.CanvasRenderingContext2D = CanvasRenderingContext2D;

    on(["canvas"], "getContext", {
      value: function (kind) {
        const wanted = String(kind).toLowerCase();
        if (wanted !== "2d") {
          // WebGL and the rest are genuinely absent, and `null` is what a
          // browser returns for a context it cannot provide — so a page's own
          // fallback branch runs, which is the behaviour the previous comment
          // here was right about and which still applies to everything but 2D.
          api.unsupported(`canvas.getContext(${String(kind)})`);
          return null;
        }
        if (!this._context2d) {
          // The surface is created at the element's current size, which is
          // what `width`/`height` reflect.
          const w = this.width || 300;
          const h = this.height || 150;
          // No reset: the surface, if one exists, is the page's and must
          // survive a second `getContext` call.
          api.canvasSize(this._id, w, h, false);
          this._context2d = new CanvasRenderingContext2D(this);
        }
        return this._context2d;
      },
      writable: true,
    });

    // `canvas.width = canvas.width` is the idiomatic erase, so the setters go
    // through to the surface rather than only to the attribute.
    for (const side of ["width", "height"]) {
      on(["canvas"], side, {
        get() {
          const raw = this.getAttribute(side);
          const parsed = raw === null ? null : parseInt(raw, 10);
          return Number.isFinite(parsed) ? parsed : (side === "width" ? 300 : 150);
        },
        set(value) {
          const next = Math.max(0, parseInt(value, 10) || 0);
          this.setAttribute(side, String(next));
          if (this._context2d) {
            // Always a reset, even when the number did not change: that is
            // what makes `canvas.width = canvas.width` the erase every page
            // uses it as.
            api.canvasSize(this._id, this.width, this.height, true);
          }
        },
      });
    }

    on(["canvas"], "toDataURL", {
      value: function (type) {
        if (type && String(type).toLowerCase() !== "image/png") {
          // Named rather than silently answering a PNG under a JPEG's name,
          // which would be a plausible wrong answer of exactly the kind this
          // engine refuses.
          api.unsupported(`canvas.toDataURL(${String(type)})`);
        }
        const url = api.canvasPng(this._id);
        // A canvas nobody drew on has no surface; a 1x1 transparent PNG is
        // what a browser gives back, and inventing one here would be less
        // honest than saying the canvas is empty.
        return url === null ? "data:," : url;
      },
      writable: true,
    });

    // The sheet an element owns. Only `<style>` and `<link>` have one, and a
    // `<link>` that is not a stylesheet has none — `img.sheet` being undefined
    // is the point of putting it here rather than on Element.
    // Forms and tables, which an agent reads more than almost anything else.
    //
    // `form.elements`, `table.rows`, `tr.cells` and `td.cellIndex` were all
    // absent, so a page that walks its own form or table — and a great deal of
    // page script does — got `undefined` and stopped.
    // ---- Numeric and picker input APIs ------------------------------------

    /// The input types that have a numeric interpretation, and how to move
    /// through them.
    ///
    /// `stepUp`, `stepDown`, `valueAsNumber` and `valueAsDate` all key off this
    /// one table, so a type is steppable in exactly one place rather than in
    /// four that can disagree.
    const STEPPABLE = {
      number: { step: 1, base: 0 },
      range: { step: 1, base: 0 },
      date: { step: 86400000, base: 0 },
      month: { step: 1, base: 0 },
      week: { step: 604800000, base: -259200000 },
      time: { step: 1000, base: 0 },
      "datetime-local": { step: 1000, base: 0 },
    };
    const DATE_VALUED = new Set(["date", "month", "week", "time"]);

    // (`inputType` — the known-types-or-text read — is declared once, beside
    // the value-mode table above.)

    /// `value` as a number, or NaN.
    ///
    /// NaN rather than `undefined` for a type that has no numeric form, which
    /// is the distinction `input-valueasnumber` checks on nearly every line:
    /// `undefined` says "this engine does not have the property", NaN says
    /// "this control has no number in it".
    function valueAsNumberOf(el) {
      const type = inputType(el);
      if (!(type in STEPPABLE)) return NaN;
      const raw = String(el.value ?? "");
      if (raw === "") return NaN;
      if (type === "number" || type === "range") {
        const n = Number(raw);
        return Number.isNaN(n) ? NaN : n;
      }
      if (type === "month") {
        const m = /^(\d{4,})-(\d{2})$/.exec(raw);
        if (!m) return NaN;
        // Months since 1970-01, which is what the spec defines this as — not
        // a timestamp, which is why it has its own branch.
        return (Number(m[1]) - 1970) * 12 + (Number(m[2]) - 1);
      }
      if (type === "time") {
        const at = Date.parse(`1970-01-01T${raw}Z`);
        return Number.isNaN(at) ? NaN : at;
      }
      if (type === "week") {
        const m = /^(\d{4,})-W(\d{2})$/.exec(raw);
        if (!m) return NaN;
        const jan4 = Date.UTC(Number(m[1]), 0, 4);
        const dow = (new Date(jan4).getUTCDay() + 6) % 7;
        return jan4 - dow * 86400000 + (Number(m[2]) - 1) * 604800000;
      }
      const at = Date.parse(type === "date" ? `${raw}T00:00:00Z` : `${raw}Z`);
      return Number.isNaN(at) ? NaN : at;
    }

    function numberToValue(el, number) {
      const type = inputType(el);
      if (type === "number" || type === "range") return String(number);
      if (type === "month") {
        const year = 1970 + Math.floor(number / 12);
        const month = ((number % 12) + 12) % 12 + 1;
        return `${String(year).padStart(4, "0")}-${String(month).padStart(2, "0")}`;
      }
      const date = new Date(number);
      if (Number.isNaN(date.getTime())) return "";
      const iso = date.toISOString();
      if (type === "date") return iso.slice(0, 10);
      if (type === "time") return iso.slice(11, 19);
      if (type === "datetime-local") return iso.slice(0, 19);
      if (type === "week") {
        const target = new Date(number);
        const day = (target.getUTCDay() + 6) % 7;
        target.setUTCDate(target.getUTCDate() - day + 3);
        const firstThursday = new Date(Date.UTC(target.getUTCFullYear(), 0, 4));
        const week = 1 + Math.round(
          (target - firstThursday) / 604800000
          - ((firstThursday.getUTCDay() + 6) % 7 - 3) / 7,
        );
        return `${target.getUTCFullYear()}-W${String(week).padStart(2, "0")}`;
      }
      return String(number);
    }

    on(["input"], "valueAsNumber", {
      get() { return valueAsNumberOf(this); },
      set(value) {
        const type = inputType(this);
        if (!(type in STEPPABLE)) {
          throw new DOMException(
            `valueAsNumber does not apply to input type=${type}`,
            "InvalidStateError",
          );
        }
        const n = Number(value);
        this.value = Number.isNaN(n) ? "" : numberToValue(this, n);
      },
    });

    on(["input"], "valueAsDate", {
      get() {
        const type = inputType(this);
        // Deliberately narrower than `valueAsNumber`: `number`, `range` and
        // `datetime-local` have no time zone, so a `Date` would be a value the
        // control does not hold. The spec says null and so does this.
        if (!DATE_VALUED.has(type)) return null;
        const n = valueAsNumberOf(this);
        if (Number.isNaN(n)) return null;
        return new Date(type === "month" ? Date.UTC(1970 + Math.floor(n / 12), ((n % 12) + 12) % 12) : n);
      },
      set(value) {
        const type = inputType(this);
        if (!DATE_VALUED.has(type)) {
          throw new DOMException(
            `valueAsDate does not apply to input type=${type}`,
            "InvalidStateError",
          );
        }
        if (value === null) { this.value = ""; return; }
        this.value = numberToValue(this, value.getTime());
      },
    });

    for (const [name, sign] of [["stepUp", 1], ["stepDown", -1]]) {
      Object.defineProperty(TAG_CLASSES.get("input").prototype, name, {
        configurable: true, writable: true,
        value(n) {
          const type = inputType(this);
          const spec = STEPPABLE[type];
          if (!spec) {
            throw new DOMException(
              `${name} does not apply to input type=${type}`,
              "InvalidStateError",
            );
          }
          const rawStep = api.getAttr(this._id, "step");
          if (rawStep !== null && rawStep.toLowerCase() === "any") {
            // "any" means there is no step, so stepping is not defined — an
            // error rather than a silent no-op, so a page stepping a slider it
            // configured that way finds out.
            throw new DOMException(`${name}: step is "any"`, "InvalidStateError");
          }
          const step = rawStep !== null && Number.isFinite(Number(rawStep)) && Number(rawStep) > 0
            ? Number(rawStep) * (type === "number" || type === "range" ? 1 : spec.step)
            : spec.step;
          const current = valueAsNumberOf(this);
          const from = Number.isNaN(current) ? spec.base : current;
          this.valueAsNumber = from + sign * step * (n === undefined ? 1 : Number(n));
        },
      });
    }

    /// `showPicker`, which is almost entirely its refusals.
    ///
    /// There is no picker to show here, so the useful half is the part a test
    /// actually checks: it throws for a type that has no picker, for a disabled
    /// or read-only control, and without a user gesture. Answering nothing at
    /// all made every one of those assertions fail.
    const PICKER_TYPES = new Set([
      "date", "month", "week", "time", "datetime-local", "color", "file",
    ]);
    Object.defineProperty(TAG_CLASSES.get("input").prototype, "showPicker", {
      configurable: true, writable: true,
      value() {
        const type = inputType(this);
        if (this.disabled || this.readOnly) {
          throw new DOMException(
            "showPicker: the control is disabled or read-only",
            "InvalidStateError",
          );
        }
        if (!PICKER_TYPES.has(type)) {
          // A type with no picker is a no-op in a browser rather than an
          // error — the exception below is for the *gesture*, which is a
          // different refusal.
          return;
        }
        throw new DOMException(
          "showPicker: this engine has no picker to show, and there is no user "
          + "gesture to attribute one to",
          "NotAllowedError",
        );
      },
    });

    // `files` is null for every type but `file`, and was undefined for all of
    // them — so a page testing `input.files` before reading it took the wrong
    // branch on a control that genuinely has no files.
    on(["input"], "files", {
      get() { return inputType(this) === "file" ? collection([], "FileList") : null; },
    });

    // ---- Text field selection ---------------------------------------------
    //
    // `selectionStart`, `selectionEnd`, `selectionDirection`,
    // `setSelectionRange`, `setRangeText` and `select` were all absent, which
    // is 464 unpassed subtests and, more to the point, the API anything that
    // edits text in a field reaches for first.
    //
    // The selection is held here rather than in the layout engine: blitz has an
    // editor for a laid-out input and this has to answer for a detached one
    // too, so the property is the truth and the editor follows it.

    /// Which controls have a selection at all.
    ///
    /// The list is exact and the exclusions are the interesting half: a
    /// `<input type=number>` reports `null` for `selectionStart` rather than 0,
    /// because it has no text selection to report — and a page that tested for
    /// `!== null` before using it was getting the wrong answer.
    const SELECTABLE_INPUT_TYPES = new Set([
      "text", "search", "url", "tel", "password",
    ]);

    function hasTextSelection(el) {
      if (el.tagName === "TEXTAREA") return true;
      if (el.tagName !== "INPUT") return false;
      return SELECTABLE_INPUT_TYPES.has((api.getAttr(el._id, "type") || "text").toLowerCase());
    }

    function selectionOf(el) {
      if (!el.__h5iSelection) {
        // 0,0 — the caret moves to the end when script assigns `value`, not
        // because the markup seeded one.
        el.__h5iSelection = { start: 0, end: 0, direction: "none" };
      }
      return el.__h5iSelection;
    }

    function clampSelection(el) {
      const length = String(el.value ?? "").length;
      const sel = selectionOf(el);
      sel.start = Math.min(sel.start, length);
      sel.end = Math.min(sel.end, length);
      if (sel.end < sel.start) sel.end = sel.start;
      return sel;
    }

    for (const [name, key] of [["selectionStart", "start"], ["selectionEnd", "end"]]) {
      on(["input", "textarea"], name, {
        get() {
          if (!hasTextSelection(this)) return null;
          return clampSelection(this)[key];
        },
        set(value) {
          if (!hasTextSelection(this)) {
            throw new DOMException(
              `${name} does not apply to this control`,
              "InvalidStateError",
            );
          }
          const sel = clampSelection(this);
          const at = Math.max(0, Math.min(Number(value) || 0, String(this.value ?? "").length));
          sel[key] = at;
          // Setting the start past the end collapses the selection there,
          // rather than leaving a range that runs backwards.
          if (sel.end < sel.start) sel.end = sel.start;
        },
      });
    }

    on(["input", "textarea"], "selectionDirection", {
      get() {
        if (!hasTextSelection(this)) return null;
        return selectionOf(this).direction;
      },
      set(value) {
        if (!hasTextSelection(this)) return;
        const wanted = String(value).toLowerCase();
        selectionOf(this).direction =
          wanted === "forward" || wanted === "backward" ? wanted : "none";
      },
    });

    for (const tag of ["input", "textarea"]) {
      const Interface = TAG_CLASSES.get(tag);
      if (!Interface) continue;
      Object.defineProperty(Interface.prototype, "setSelectionRange", {
        configurable: true, writable: true,
        value(start, end, direction) {
          if (!hasTextSelection(this)) {
            throw new DOMException(
              "setSelectionRange does not apply to this control",
              "InvalidStateError",
            );
          }
          const length = String(this.value ?? "").length;
          const clamp = (n) => Math.max(0, Math.min(Number(n) || 0, length));
          const from = clamp(start);
          const to = Math.max(from, clamp(end));
          this.__h5iSelection = {
            start: from,
            end: to,
            direction: direction === "forward" || direction === "backward"
              ? direction
              : "none",
          };
          this.dispatchEvent(new Event("select", { bubbles: true }));
        },
      });
      Object.defineProperty(Interface.prototype, "select", {
        configurable: true, writable: true,
        value() {
          if (!hasTextSelection(this)) return;
          this.setSelectionRange(0, String(this.value ?? "").length);
        },
      });
      Object.defineProperty(Interface.prototype, "setRangeText", {
        configurable: true, writable: true,
        value(replacement, start, end, selectMode) {
          if (!hasTextSelection(this)) {
            throw new DOMException(
              "setRangeText does not apply to this control",
              "InvalidStateError",
            );
          }
          const text = String(this.value ?? "");
          const sel = clampSelection(this);
          // Two arities: with no range it replaces the current selection,
          // which is what an editor toolbar calls.
          const from = start === undefined ? sel.start : Math.max(0, Math.min(Number(start) || 0, text.length));
          const to = end === undefined ? sel.end : Math.max(0, Math.min(Number(end) || 0, text.length));
          if (from > to) {
            throw new DOMException("setRangeText: start is past end", "IndexSizeError");
          }
          const inserted = String(replacement ?? "");
          this.value = text.slice(0, from) + inserted + text.slice(to);
          const mode = String(selectMode ?? "preserve").toLowerCase();
          if (mode === "select") {
            this.__h5iSelection = { start: from, end: from + inserted.length, direction: "none" };
          } else if (mode === "start") {
            this.__h5iSelection = { start: from, end: from, direction: "none" };
          } else if (mode === "end") {
            const at = from + inserted.length;
            this.__h5iSelection = { start: at, end: at, direction: "none" };
          } else {
            // `preserve`: the selection moves with the text around it, which
            // is why the offsets are shifted rather than reset.
            const delta = inserted.length - (to - from);
            this.__h5iSelection = {
              start: sel.start > to ? sel.start + delta : Math.min(sel.start, from),
              end: sel.end > to ? sel.end + delta : Math.min(sel.end, from + inserted.length),
              direction: sel.direction,
            };
          }
        },
      });
    }

    // ---- Constraint validation ------------------------------------------
    //
    // `html/semantics/forms/constraints` scored **1 of 920**, and the reason
    // was not that the feature is subtle: none of it existed. `validity`,
    // `willValidate`, `checkValidity`, `reportValidity`, `setCustomValidity`
    // and `validationMessage` were all absent, so every test failed on
    // "The validity attribute doesn't exist" before reaching what it meant to
    // check.
    //
    // It is also the API a page uses to decide whether to submit, which makes
    // it one an agent driving a form needs to be able to read.

    /// Which elements are *candidates* for constraint validation.
    ///
    /// The barred cases are not an optimisation: a disabled control that
    /// reported itself invalid would block a form the user cannot even reach,
    /// and a `<button type=button>` is not submitted at all.
    const NEVER_VALIDATED_INPUT_TYPES = new Set(["hidden", "reset", "button"]);

    function isSubmittable(el) {
      return ["INPUT", "SELECT", "TEXTAREA", "BUTTON"].includes(el.tagName);
    }

    function isBarredFromValidation(el) {
      if (!isSubmittable(el)) return true;
      if (el.disabled) return true;
      if (el.tagName === "INPUT") {
        const type = (api.getAttr(el._id, "type") || "text").toLowerCase();
        if (NEVER_VALIDATED_INPUT_TYPES.has(type)) return true;
        if (el.readOnly) return true;
      }
      if (el.tagName === "TEXTAREA" && el.readOnly) return true;
      if (el.tagName === "BUTTON") {
        const type = (api.getAttr(el._id, "type") || "submit").toLowerCase();
        if (type !== "submit") return true;
      }
      // A control inside a `<datalist>` is a suggestion, not an entry.
      for (let node = el.parentNode; node; node = node.parentNode) {
        if (node.tagName === "DATALIST") return true;
      }
      return false;
    }

    const EMAIL = /^[^\s@]+@[^\s@.]+(\.[^\s@.]+)+$/;

    /// The numeric value of a control, for the range and step constraints.
    ///
    /// `null` when the type has no numeric interpretation, which is what keeps
    /// `min`/`max` from being applied to a plain text field.
    function numericValue(el, raw) {
      const type = (api.getAttr(el._id, "type") || "text").toLowerCase();
      if (["number", "range"].includes(type)) {
        const n = Number(raw);
        return raw === "" || Number.isNaN(n) ? null : n;
      }
      if (["date", "month", "week", "time", "datetime-local"].includes(type)) {
        const at = Date.parse(type === "time" ? `1970-01-01T${raw}Z` : raw);
        return Number.isNaN(at) ? null : at;
      }
      return null;
    }

    function computeValidity(el) {
      const flags = {
        valueMissing: false, typeMismatch: false, patternMismatch: false,
        tooLong: false, tooShort: false, rangeUnderflow: false,
        rangeOverflow: false, stepMismatch: false, badInput: false,
        customError: !!el.__h5iCustomError,
      };
      if (isBarredFromValidation(el)) {
        // Barred elements are always valid, *including* when a custom error
        // was set: the element is not a candidate, so nothing about it is
        // checked. This is the clause that stops a hidden field blocking a
        // form nobody can fix.
        return { ...flags, customError: false, valid: true };
      }

      const type = el.tagName === "INPUT"
        ? (api.getAttr(el._id, "type") || "text").toLowerCase()
        : el.tagName.toLowerCase();
      const value = el.tagName === "SELECT" || el.tagName === "BUTTON" ? el.value : (el.value ?? "");
      const required = api.getAttr(el._id, "required") !== null;

      if (required) {
        if (type === "checkbox") flags.valueMissing = !el.checked;
        else if (type === "radio") {
          const name = el.name;
          const group = name
            ? document.querySelectorAll(`input[type=radio][name="${name}"]`)
            : [el];
          flags.valueMissing = !group.some((other) => other.checked);
        } else flags.valueMissing = value === "";
      }

      if (value !== "") {
        if (type === "email") flags.typeMismatch = !EMAIL.test(value);
        else if (type === "url") {
          flags.typeMismatch = api.parseUrl(value, "") === null;
        }

        const pattern = api.getAttr(el._id, "pattern");
        if (pattern !== null && ["text", "search", "url", "tel", "email", "password"].includes(type)) {
          try {
            // Anchored, and `v` rather than `u` is not available here — the
            // whole value must match, which is the difference between
            // `pattern` and a search.
            flags.patternMismatch = !new RegExp(`^(?:${pattern})$`, "u").test(value);
          } catch {
            // An unparseable pattern is ignored rather than treated as a
            // mismatch, which is what the spec says and stops one typo in an
            // attribute making a form unsubmittable.
          }
        }

        // Length constraints apply only once the value has been edited, which
        // is what the spec's "dirty value flag" means. `_value` is set by the
        // property setter and by `Page::type_into`, so it is exactly that flag.
        const dirty = el._value !== undefined;
        const maxLength = Number(api.getAttr(el._id, "maxlength"));
        const minLength = Number(api.getAttr(el._id, "minlength"));
        if (dirty && Number.isInteger(maxLength) && maxLength >= 0) {
          flags.tooLong = value.length > maxLength;
        }
        if (dirty && Number.isInteger(minLength) && minLength >= 0) {
          flags.tooShort = value.length < minLength;
        }

        const numeric = numericValue(el, value);
        if (numeric !== null) {
          const min = numericValue(el, api.getAttr(el._id, "min") ?? "");
          const max = numericValue(el, api.getAttr(el._id, "max") ?? "");
          if (min !== null) flags.rangeUnderflow = numeric < min;
          if (max !== null) flags.rangeOverflow = numeric > max;
          const stepRaw = api.getAttr(el._id, "step");
          if (stepRaw !== null && stepRaw.toLowerCase() !== "any") {
            const step = Number(stepRaw);
            if (Number.isFinite(step) && step > 0) {
              const base = min ?? 0;
              const offset = Math.abs((numeric - base) % step);
              // A floating-point remainder is never exactly zero, so the
              // comparison has to have a tolerance or every decimal step
              // mismatches.
              flags.stepMismatch = offset > 1e-9 && Math.abs(offset - step) > 1e-9;
            }
          }
        }
      }

      const valid = !Object.keys(flags).some((k) => flags[k]);
      return { ...flags, valid };
    }

    on(["input", "select", "textarea", "button", "fieldset", "output", "object"],
      "willValidate", { get() { return !isBarredFromValidation(this); } });

    // A real interface, not a frozen snapshot: `ValidityState` is a global
    // idlharness checks by name, and its getters are *live* — a page that
    // holds `input.validity` and types into the field reads the new answer
    // through the old reference, exactly as in a browser.
    class ValidityState {
      constructor() { throw new TypeError("Illegal constructor"); }
    }
    for (const flag of [
      "valueMissing", "typeMismatch", "patternMismatch", "tooLong", "tooShort",
      "rangeUnderflow", "rangeOverflow", "stepMismatch", "badInput",
      "customError", "valid",
    ]) {
      const getter = function () {
        const el = this && this.__h5iControl;
        if (!el) {
          throw new TypeError(`Illegal invocation: ${flag} needs a ValidityState`);
        }
        return computeValidity(el)[flag] ?? false;
      };
      Object.defineProperty(getter, "name", { value: `get ${flag}` });
      Object.defineProperty(ValidityState.prototype, flag, {
        configurable: true, enumerable: true, get: getter,
      });
    }
    Object.defineProperty(ValidityState.prototype, Symbol.toStringTag, {
      value: "ValidityState", configurable: true,
    });
    globalThis.ValidityState = ValidityState;

    on(["input", "select", "textarea", "button", "fieldset", "output", "object"],
      "validity", {
        get() {
          const state = Object.create(ValidityState.prototype);
          Object.defineProperty(state, "__h5iControl", { value: this });
          return state;
        },
      });

    const VALIDATION_MESSAGES = {
      valueMissing: "Please fill out this field.",
      typeMismatch: "Please enter a value of the correct type.",
      patternMismatch: "Please match the requested format.",
      tooLong: "Please shorten this text.",
      tooShort: "Please lengthen this text.",
      rangeUnderflow: "Value must be greater than or equal to the minimum.",
      rangeOverflow: "Value must be less than or equal to the maximum.",
      stepMismatch: "Please enter a valid value.",
      badInput: "Please enter a valid value.",
    };

    on(["input", "select", "textarea", "button", "fieldset", "output", "object"],
      "validationMessage", {
        get() {
          if (isBarredFromValidation(this)) return "";
          const flags = computeValidity(this);
          if (flags.valid) return "";
          if (flags.customError) return this.__h5iCustomError || "";
          for (const key of Object.keys(VALIDATION_MESSAGES)) {
            if (flags[key]) return VALIDATION_MESSAGES[key];
          }
          return "";
        },
      });

    for (const tag of ["input", "select", "textarea", "button", "fieldset", "output", "object"]) {
      const Interface = TAG_CLASSES.get(tag);
      if (!Interface) continue;
      Object.defineProperty(Interface.prototype, "setCustomValidity", {
        configurable: true, writable: true,
        value(message) {
          // The empty string clears it, which is how a page says "this is fine
          // now" — storing "" as an error would leave the control permanently
          // invalid.
          this.__h5iCustomError = String(message ?? "") || undefined;
        },
      });
      Object.defineProperty(Interface.prototype, "checkValidity", {
        configurable: true, writable: true,
        value() {
          if (isBarredFromValidation(this)) return true;
          if (computeValidity(this).valid) return true;
          // `invalid` is cancelable and does not bubble. A page listens for it
          // to put its own message beside the field, which is most of what
          // this API is used for.
          this.dispatchEvent(new Event("invalid", { cancelable: true }));
          return false;
        },
      });
      Object.defineProperty(Interface.prototype, "reportValidity", {
        configurable: true, writable: true,
        // Identical to `checkValidity` here: the difference is that a browser
        // *shows* the message, and this engine has no UI to show it in. Said
        // rather than left implicit, because a page calling this expects the
        // return value and not the bubble.
        value() { return this.checkValidity(); },
      });
    }

    on(["form"], "elements", {
      get() {
        // Ownership, not containment. A control with `form="thisId"` belongs
        // here however far away it sits, and a descendant with `form=` naming
        // something else does not — which is exactly what the attribute is
        // for, and what a descendant-only query gets backwards both ways.
        const owned = document
          .querySelectorAll("input, select, textarea, button, fieldset, output")
          .filter((el) => sameNode(formOwnerOf(el), this))
          .filter((el) => (api.getAttr(el._id, "type") || "").toLowerCase() !== "image");
        return collection(owned, "HTMLFormControlsCollection");
      },
    });
    on(["form"], "length", { get() { return this.elements.length; } });
    // Enumerated, and its missing-value default is `on` rather than "": a page
    // branching on it has two states to handle, not three.
    on(["form"], "autocomplete", {
      get() {
        return (api.getAttr(this._id, "autocomplete") || "").toLowerCase() === "off"
          ? "off"
          : "on";
      },
      set(value) { this.setAttribute("autocomplete", String(value)); },
    });

    /// `requestSubmit` and `submit`, which differ in the two ways that matter.
    Object.defineProperty(TAG_CLASSES.get("form").prototype, "requestSubmit", {
      configurable: true, writable: true,
      value(submitter) {
        if (submitter !== undefined && submitter !== null) {
          const type = submitter.tagName === "BUTTON"
            ? (api.getAttr(submitter._id, "type") || "submit").toLowerCase()
            : (api.getAttr(submitter._id, "type") || "").toLowerCase();
          if (!(submitter.tagName === "BUTTON" && type === "submit")
            && !(submitter.tagName === "INPUT" && (type === "submit" || type === "image"))) {
            throw new TypeError("requestSubmit: the submitter is not a submit button");
          }
          const owner = submitter.form;
          if (!owner || owner._id !== this._id) {
            throw new DOMException(
              "requestSubmit: the submitter is not owned by this form",
              "NotFoundError",
            );
          }
        }
        // Interactively validate, unless the form opted out. A form that fails
        // here fires `invalid` on each bad control and never fires `submit`,
        // which is the sequence a page listening for both depends on.
        if (!this.noValidate && !this.reportValidity()) return;
        const event = new SubmitEvent("submit", {
          bubbles: true, cancelable: true, submitter: submitter ?? null,
        });
        this.dispatchEvent(event);
        if (event.defaultPrevented) return;
        submitTheForm(this, submitter ?? null);
      },
    });
    Object.defineProperty(TAG_CLASSES.get("form").prototype, "submit", {
      configurable: true, writable: true,
      value() {
        // No validation and no `submit` event, deliberately. See above.
        submitTheForm(this, null);
      },
    });

    // Reset fires its event first and asks: a `reset` listener that calls
    // `preventDefault()` keeps every field as it is. The reset itself is
    // dropping the dirty state — `_value` and `_checked` are the overlays
    // script and typing put over the attributes, and the attributes *are* the
    // defaults the spec says to return to.
    Object.defineProperty(TAG_CLASSES.get("form").prototype, "reset", {
      configurable: true,
      writable: true,
      value() {
        const ev = new Event("reset", { bubbles: true, cancelable: true });
        this.dispatchEvent(ev);
        if (ev.defaultPrevented) return;
        for (const control of this.elements) {
          delete control._value;
          delete control._checked;
        }
      },
    });
    // A form validates by asking each of its controls, and the *statically
    // validate* step is what makes this more than a loop: every control is
    // checked and every invalid one gets its `invalid` event, rather than
    // stopping at the first. A page that highlights all its bad fields at once
    // depends on that.
    for (const name of ["checkValidity", "reportValidity"]) {
      Object.defineProperty(TAG_CLASSES.get("form").prototype, name, {
        configurable: true, writable: true,
        value() {
          let ok = true;
          for (const control of this.elements) {
            if (typeof control.checkValidity !== "function") continue;
            if (!control.checkValidity()) ok = false;
          }
          return ok;
        },
      });
    }
    // A form's default method is `get`, not the empty string: code branches on
    // it, and "" is not one of the branches.
    on(["form"], "method", {
      get() {
        const raw = (api.getAttr(this._id, "method") || "").toLowerCase();
        return raw === "post" || raw === "dialog" ? raw : "get";
      },
      set(value) { this.setAttribute("method", String(value)); },
    });

    // The form a control belongs to: its `form` attribute if it names one,
    // otherwise the form it sits inside.
    /// Whether two wrappers name the same node.
    ///
    /// **Not `===`.** `wrap()` hands back the `observed` proxy while a getter
    /// runs with the raw target as `this` (see `observed`, which passes the
    /// target as the receiver deliberately), so a proxy and its target are two
    /// objects for the same node. Comparing them by identity silently answered
    /// "different" for every element — `form.elements` came back empty, and an
    /// empty entry list is a form that submits nothing.
    const sameNode = (a, b) => !!a && !!b && a._id === b._id;

    /// A control's *form owner*, which is not simply its nearest ancestor form.
    function formOwnerOf(el) {
      const named = api.getAttr(el._id, "form");
      if (named !== null) {
        if (named === "") return null;
        const found = wrap(api.query("#" + cssEscapeIdent(named), 0));
        return found && found.tagName === "FORM" ? found : null;
      }
      for (let n = el.parentNode; n; n = n.parentNode) {
        if (n.nodeType === 1 && n.tagName === "FORM") return n;
      }
      return null;
    }

    on(["input", "select", "textarea", "button", "fieldset", "output", "label", "object"],
      "form", { get() { return formOwnerOf(this); } });

    // `<option>.text` is its text with whitespace collapsed, which is what a
    // `<select>` actually shows.
    on(["option"], "text", {
      get() { return (this.textContent || "").replace(/\s+/g, " ").trim(); },
      set(value) { this.textContent = String(value); },
    });
    on(["option"], "index", {
      get() {
        const owner = this.parentNode;
        if (!owner) return 0;
        return owner.querySelectorAll("option").findIndex((o) => o._id === this._id);
      },
    });
    on(["select"], "selectedIndex", {
      get() {
        const options = this.querySelectorAll("option");
        const at = options.findIndex((o) => o.selected);
        // A `<select>` with nothing marked selected shows its first option.
        return at >= 0 ? at : (options.length ? 0 : -1);
      },
      set(index) {
        const options = this.querySelectorAll("option");
        options.forEach((o, i) => { o.selected = i === Number(index); });
      },
    });
    on(["select"], "options", {
      get() { return collection(this.querySelectorAll("option"), "HTMLOptionsCollection"); },
    });

    // Tables. `rows` spans the sections in document order, which is what the
    // spec says and what a reader expects.
    on(["table"], "rows", {
      get() { return collection(this.querySelectorAll("tr"), "HTMLCollection"); },
    });
    on(["table"], "tBodies", {
      get() { return collection(this.querySelectorAll("tbody"), "HTMLCollection"); },
    });
    on(["table"], "tHead", { get() { return this.querySelector("thead"); } });
    on(["table"], "tFoot", { get() { return this.querySelector("tfoot"); } });
    on(["table"], "caption", { get() { return this.querySelector("caption"); } });
    on(["tr"], "cells", {
      get() { return collection(this.querySelectorAll("td, th"), "HTMLCollection"); },
    });
    on(["tr"], "rowIndex", {
      get() {
        for (let n = this.parentNode; n; n = n.parentNode) {
          if (n.nodeType === 1 && n.tagName === "TABLE") {
            return n.querySelectorAll("tr").findIndex((r) => r._id === this._id);
          }
        }
        return -1;
      },
    });
    on(["td"], "cellIndex", {
      get() {
        const row = this.parentNode;
        if (!row || row.tagName !== "TR") return -1;
        return row.querySelectorAll("td, th").findIndex((c) => c._id === this._id);
      },
    });

    on(["style", "link"], "sheet", {
      get() {
        if (this.tagName === "LINK") {
          const rel = (api.getAttr(this._id, "rel") || "").toLowerCase();
          if (!rel.split(/\s+/).includes("stylesheet")) return null;
        }
        return CSSStyleSheet.forElement(this);
      },
    });
  }

  const HANDLER_EVENTS = [
    "click", "dblclick", "mousedown", "mouseup", "mouseover", "mouseout", "mousemove",
    "input", "change", "submit", "focus", "blur", "keydown", "keyup", "keypress",
    "load", "error", "scroll", "wheel", "contextmenu", "pointerdown", "pointerup",
    "touchstart", "touchend", "animationend", "transitionend",
    // The open/close vocabulary: popovers and dialogs announce themselves
    // through these four, and `command` is how an invoker button reaches the
    // element it points at.
    "toggle", "beforetoggle", "cancel", "close", "command",
    // The rest of GlobalEventHandlers. Declaring the property is not claiming
    // the engine *fires* the event — most of these never fire here — but the
    // accessor pair is what `el.onpaste = fn` needs to at least register, and
    // idlharness checks every name on every element interface.
    "abort", "auxclick", "beforeinput", "beforematch", "canplay",
    "canplaythrough", "contextlost", "contextrestored", "copy", "cuechange",
    "cut", "drag", "dragend", "dragenter", "dragleave", "dragover",
    "dragstart", "drop", "durationchange", "emptied", "ended", "formdata",
    "invalid", "loadeddata", "loadedmetadata", "loadstart", "mouseenter",
    "mouseleave", "paste", "pause", "play", "playing", "progress",
    "ratechange", "reset", "resize", "scrollend", "securitypolicyviolation",
    "seeked", "seeking", "select", "slotchange", "stalled", "suspend",
    "timeupdate", "volumechange", "waiting", "pointermove",
    "pointerover", "pointerout", "pointerenter", "pointerleave",
    "pointercancel", "gotpointercapture", "lostpointercapture",
    "animationstart", "animationiteration", "animationcancel",
    "transitionrun", "transitionstart", "transitioncancel", "selectstart",
    "selectionchange", "touchmove", "touchcancel",
  ];
  for (const type of HANDLER_EVENTS) {
    const slot = `__on_${type}`;
    Object.defineProperty(Element.prototype, `on${type}`, {
      configurable: true,
      get() { return this[slot] ?? null; },
      set(handler) {
        // Assigning replaces whatever the property held before, which is what
        // makes it different from `addEventListener` — two assignments leave
        // one handler, not two.
        if (this[slot]) this.removeEventListener(type, this[slot]);
        this[slot] = typeof handler === "function" ? handler : null;
        if (this[slot]) this.addEventListener(type, this[slot]);
      },
    });
  }

  /// The window's own `on*` properties.
  ///
  /// `window.onload = fn` stored the function and never called it. The
  /// accessors above are installed on `Element.prototype`, and the window is
  /// not an element, so the assignment landed on an ordinary expando: it read
  /// back correctly, which is why nothing looked wrong, and it never ran.
  ///
  /// These delegate to the window's own `addEventListener`, so a page that
  /// mixes the two forms gets one handler per assignment and no double-fire.
  const WINDOW_HANDLER_EVENTS = [
    "load", "unload", "beforeunload", "error", "message", "messageerror",
    "hashchange", "popstate", "pagehide", "pageshow", "resize", "scroll",
    "storage", "offline", "online", "languagechange", "rejectionhandled",
    "unhandledrejection", "afterprint", "beforeprint",
  ];
  function installWindowHandler(type) {
    const slot = `__on_window_${type}`;
    Object.defineProperty(globalThis, `on${type}`, {
      configurable: true,
      // Enumerable like every other WebIDL attribute — the window's handler
      // properties are members of Window, not engine internals.
      enumerable: true,
      get() { return globalThis[slot] ?? null; },
      set(handler) {
        if (globalThis[slot]) removeEventListener(type, globalThis[slot]);
        globalThis[slot] = typeof handler === "function" ? handler : null;
        if (globalThis[slot]) addEventListener(type, globalThis[slot]);
      },
    });
  }
  for (const type of WINDOW_HANDLER_EVENTS) installWindowHandler(type);
  /// Window includes GlobalEventHandlers as well, so `"onsubmit" in window` is
  /// true here as it is in a browser. Not only for the IDL: a false sent
  /// jQuery 1.x down its IE branch, which is where it used to die.
  ///
  /// Kept out of `WINDOW_HANDLER_EVENTS`, which is also the set `<body>`
  /// forwards to the window — `body.onclick` is the body's own.
  for (const type of HANDLER_EVENTS) {
    if (!WINDOW_HANDLER_EVENTS.includes(type)) installWindowHandler(type);
  }
  // `window.name` is a plain settable string here. In a browser it names the
  // browsing context for `target=`; this engine has one context (§B20.15), so
  // the value round-trips and nothing else reads it.
  {
    let windowName = "";
    Object.defineProperty(globalThis, "name", {
      configurable: true,
      enumerable: true,
      get() { return windowName; },
      set(value) { windowName = String(value); },
    });
  }
  // The WindowEventHandlers accessors that `<body>` and `<frameset>` carry on
  // their *prototypes* but hold on behalf of the window — the IDL-attribute
  // half of BODY_FORWARDED below: assigning `document.body.onhashchange` and
  // assigning `window.onhashchange` are the same storage.
  for (const tag of ["body", "frameset"]) {
    const Interface = TAG_CLASSES.get(tag);
    if (!Interface) continue;
    for (const type of WINDOW_HANDLER_EVENTS) {
      Object.defineProperty(Interface.prototype, `on${type}`, {
        configurable: true,
        enumerable: true,
        get() { return globalThis[`on${type}`] ?? null; },
        set(handler) { globalThis[`on${type}`] = handler; },
      });
    }
  }

  /// Event-handler *content attributes*: `<body onload="run()">`.
  const HANDLER_ATTRS = [
    ...HANDLER_EVENTS, ...WINDOW_HANDLER_EVENTS,
    "focusin", "focusout", "readystatechange",
  ];
  const HANDLER_ATTR_SET = new Set(HANDLER_ATTRS.map((type) => `on${type}`));
  const HANDLER_ATTR_SELECTOR = HANDLER_ATTRS.map((type) => `[on${type}]`).join(",");

  /// The handlers `<body>` and `<frameset>` do not keep for themselves.
  ///
  /// The spec forwards this set to the window, and the difference is not
  /// cosmetic: `load` is fired *at* the window, so a `<body onload>` installed
  /// on the body element would sit through the one event it exists for.
  const BODY_FORWARDED = new Set([
    "blur", "error", "focus", "load", "resize", "scroll", "afterprint",
    "beforeprint", "beforeunload", "hashchange", "languagechange", "message",
    "messageerror", "offline", "online", "pagehide", "pageshow", "popstate",
    "rejectionhandled", "storage", "unhandledrejection", "unload",
  ]);

  function installInlineHandler(element, name, source) {
    const type = name.slice(2);
    let compiled;
    try {
      compiled = new Function("event", source);
    } catch (error) {
      // A handler that does not parse is the page's bug, not this engine's, and
      // a browser reports it and carries on rather than taking the document
      // down with it.
      console.error(`inline ${name} did not compile: ${error}`);
      return;
    }
    const handler = function (event) { return compiled.call(element, event); };
    const tag = element.tagName;
    if ((tag === "BODY" || tag === "FRAMESET") && BODY_FORWARDED.has(type)) {
      globalThis[`on${type}`] = handler;
      return;
    }
    if (`on${type}` in element) element[`on${type}`] = handler;
    else element.addEventListener(type, handler);
  }

  /// Compile the inline handlers under `within`, or under the whole document.
  ///
  /// Idempotent by remembering the source it last compiled per attribute, so
  /// the sweep can run again after markup arrives without stacking a second
  /// copy of every handler on the elements it already saw.
  globalThis.__h5iInstallInlineHandlers = function (within) {
    const scope = within && within._id ? within._id : 0;
    for (const id of api.queryAll(HANDLER_ATTR_SELECTOR, scope)) {
      const element = wrap(id);
      if (!element) continue;
      const installed = element.__h5iInline ?? (element.__h5iInline = {});
      for (const name of api.attrNames(id) ?? []) {
        const lowered = String(name).toLowerCase();
        if (!HANDLER_ATTR_SET.has(lowered)) continue;
        const source = api.getAttr(id, name);
        if (source == null || installed[lowered] === source) continue;
        installed[lowered] = source;
        installInlineHandler(element, lowered, source);
      }
    }
  };

  /// The elements that fetch something, and the attribute that names it.
  ///
  /// Neither `load` nor `error` was ever dispatched on these: the fetch
  /// happened, the receipt was written, and nothing told the element. So
  /// `<img src=x onerror=…>` did nothing, and a real finding read as none.
  const RESOURCE_ATTR = { OBJECT: "data", LINK: "href" };
  const RESOURCE_SELECTOR =
    "img[src],input[src],script[src],link[href],iframe[src],frame[src]," +
    "embed[src],object[data],source[src],track[src],audio[src],video[src]";

  /// Deliver `load` and `error` for the subresources that have resolved.
  ///
  /// From the document's own record (`api.resourceStatus`), never a second
  /// request: an element not in there yet is left for the next pass rather than
  /// guessed about. Once per URL an element holds, so re-running is free and a
  /// changed `src` re-arms. Returns whether anything was dispatched.
  globalThis.__h5iFireResourceEvents = function () {
    let fired = false;
    for (const id of api.queryAll(RESOURCE_SELECTOR, 0)) {
      const element = wrap(id);
      if (!element) continue;
      // A dynamic `<script>` goes through the loader, which fires its own pair.
      if (element.tagName === "SCRIPT" && element.__h5iScriptStarted) continue;
      const raw = api.getAttr(id, RESOURCE_ATTR[element.tagName] ?? "src");
      if (!raw || element.__h5iResourceFor === raw) continue;
      let resolved;
      try {
        resolved = new URL(raw, document.baseURI).href;
      } catch {
        continue;
      }
      const status = api.resourceStatus(resolved);
      if (status === null) continue;
      element.__h5iResourceFor = raw;
      // Outside 2xx is a resource the page did not get; `0` is one that got no
      // answer. Both are `error`, and which it was is in the receipt.
      element.dispatchEvent(new Event(status >= 200 && status < 300 ? "load" : "error"));
      fired = true;
    }
    // `<svg>` fires `load` once in the document, waiting on no resource of its
    // own, which is why no subresource bookkeeping reaches `<svg onload=…>`.
    for (const id of api.queryAll("svg", 0)) {
      const element = wrap(id);
      if (!element || element.__h5iSvgLoaded) continue;
      element.__h5iSvgLoaded = true;
      element.dispatchEvent(new Event("load"));
      fired = true;
    }
    return fired;
  };

  /// Turn every `<template shadowrootmode>` inside `within` into a shadow root.
  ///
  /// Order matters and is the fiddly part: `attachShadow` takes the host's
  /// light children out of the way first, so the template has to be removed
  /// *before* the root is attached, or the template itself would be filed as
  /// light content of the component it was supposed to become.
  function adoptDeclarativeShadowRoots(within) {
    const templates = api
      .queryAll("template[shadowrootmode]", within._id)
      .map(wrap)
      .filter(Boolean);
    for (const template of templates) {
      const host = template.parentNode;
      if (!host || host._shadow || host.nodeType !== 1) continue;
      const mode = (api.getAttr(template._id, "shadowrootmode") || "open").toLowerCase();
      const content = [...template.childNodes];
      for (const node of content) detachFromParent(node);
      template.remove();
      const root = host.attachShadow({ mode: mode === "closed" ? "closed" : "open" });
      for (const node of content) root.appendChild(node);
    }
  }

  /// Refuse a selector a browser would refuse, rather than answering "nothing".
  const POPOVER_OPEN_CLASS = "__h5i_popover_open__";
  /// Same arrangement for `:modal`: `showModal` stamps it, `close` lifts it.
  const MODAL_OPEN_CLASS = "__h5i_modal_open__";

  /// `:heading` and `:heading(n)`, rewritten to the tags they mean.
  ///
  /// Selectors 4 adds them and Stylo 0.19 rejects both, so every use was a
  /// SyntaxError — 277 subtests across `css/selectors/heading` and the
  /// `headingoffset` files, every one of which reported the selector as
  /// invalid rather than as not matching. The same textual-rewrite road
  /// `:popover-open` and `:modal` already take, and cheaper than either
  /// because there is no state to stamp: `:heading` *is* h1 through h6.
  ///
  /// A level outside 1-6 selects nothing — `:heading(7)` and `:heading(0)`
  /// are well-formed and match no element — so they become `:not(*)` rather
  /// than an error. `hgroup` and `role="heading"` are deliberately not
  /// included; the suite asserts both do **not** match.
  const HEADING_TAGS = ":is(h1,h2,h3,h4,h5,h6)";

  /// `null` when the argument is not a plain list of integers.
  ///
  /// Selectors 4 allows `<An+B>#`, so `:heading(2n+1)` is well-formed and this
  /// does not implement it. Rewriting it to `:not(*)` would answer "matches
  /// nothing", which is a **plausible wrong answer** — the caller cannot tell
  /// it from a selector that genuinely matched nothing. Returning null leaves
  /// the selector untouched, so the parser rejects it and the page gets the
  /// SyntaxError that says this engine does not know the form.
  function headingLevels(argument) {
    const parts = String(argument).split(",").map((one) => one.trim());
    if (!parts.every((one) => /^[+-]?\d+$/.test(one))) return null;
    const levels = parts.map(Number)
      .filter((level) => level >= 1 && level <= 6);
    // An in-range-free but well-formed list — `:heading(7)` — matches nothing,
    // and that *is* the right answer for it.
    return levels.length ? `:is(${levels.map((n) => `h${n}`).join(",")})` : ":not(*)";
  }

  function checkSelector(selector) {
    const text = String(selector)
      .replace(/:heading\(([^)]*)\)/gi, (whole, argument) => headingLevels(argument) ?? whole)
      .replace(/:heading\b/gi, HEADING_TAGS)
      .replace(/:popover-open\b/g, "." + POPOVER_OPEN_CLASS)
      .replace(/:modal\b/g, "." + MODAL_OPEN_CLASS);
    if (!api.validSelector(text)) {
      throw new DOMException(`${text} is not a valid selector`, "SyntaxError");
    }
    return text;
  }

  // ── `:has()`, evaluated here rather than parsed there ─────────────────────
  //
  // Stylo's servo selector parser hardcodes `parse_has() -> false`, and the
  // owner's decision stands that this repo will not carry a patched copy of
  // stylo to change that. So the *query* half of `:has()` is evaluated in
  // this file instead, with the engine's own matcher doing all the actual
  // matching: each `:has(ARG)` group is computed into a transient marker
  // class on the elements that have a relative match, the selector is
  // rewritten to that class, the ordinary query runs, and the markers are
  // removed before the call returns — written with the raw attribute ops so
  // no MutationObserver and no attributeChangedCallback ever sees them.
  //
  // What this covers: querySelector/querySelectorAll/matches/closest — every
  // path that funnels through `checkSelector`. What it does not:
  // **stylesheet rules** using `:has()`, which go through Stylo's parser
  // inside Blitz and are silently dropped there; that half stays a named gap
  // until a Blitz release depends on stylo >= 0.20 (see ROADMAP §B22.1).
  const HAS_PATTERN = /:has\(/i;

  // The evaluator itself is in `prelude/has.js`, parsed the first time a
  // selector actually contains `:has(`. Every selector-taking API already asked
  // that question to decide whether it needed any of this; now the same
  // question decides whether the file exists yet.
  internals.wrap = wrap;
  internals.HAS_PATTERN = HAS_PATTERN;

  /// The wrapper every selector-taking API goes through: plain selectors go
  /// straight to `checkSelector`, `:has()` selectors get their markers for
  /// exactly the duration of `run`.
  function withHasMarkers(selector, run) {
    const text = String(selector);
    if (!HAS_PATTERN.test(text)) return run(checkSelector(text));
    if (!internals.prepareHasSelector) __h5iTier("has");
    const { rewritten, cleanup } = internals.prepareHasSelector(text);
    try {
      return run(checkSelector(rewritten));
    } finally {
      cleanup();
    }
  }

  function camelToDash(name) {
    return name.replace(/[A-Z]/g, (c) => "-" + c.toLowerCase());
  }

  /// One declaration's **serialised** value, memoised on what it was given.
  const serializedValues = new Map();
  /// Parsed declaration maps, keyed on the `style` text they came from.
  const declarationMaps = new Map();

  function serializedValue(property, raw) {
    if (raw === undefined) return "";
    // A custom property is whatever the page put there; there is nothing to
    // normalise and the parser would decline it anyway.
    if (property.startsWith("--")) return raw;
    const key = `${property}\u0000${raw}`;
    const known = serializedValues.get(key);
    if (known !== undefined) return known;
    const value = api.serializeCssValue(property, raw) || raw;
    if (serializedValues.size > 4096) serializedValues.clear();
    serializedValues.set(key, value);
    return value;
  }

  // Inline style, backed by the element's own `style` attribute rather than by
  // a parallel object, so what script sets is what the cascade sees and what a
  // later `getAttribute("style")` returns. One source of truth, same rule the
  // DOM follows.
  class StyleDeclaration {
    /// `source` is a get/set pair for the declaration *text*.
    ///
    /// An element's inline style reads and writes its `style` attribute; a rule
    /// inside a stylesheet reads and writes its own body. One parser and one
    /// serialiser for both, rather than a second copy that could disagree with
    /// this one about what `color:red;;` means.
    constructor(source) { this._source = source; }

    /// The declarations, as the author wrote them.
    _read() {
      const raw = this._source.get();
      const known = declarationMaps.get(raw);
      if (known !== undefined) return known;
      const out = new Map();
      for (const part of raw.split(";")) {
        const at = part.indexOf(":");
        if (at < 0) continue;
        const name = part.slice(0, at).trim().toLowerCase();
        if (name) out.set(name, part.slice(at + 1).trim());
      }
      if (declarationMaps.size > 512) declarationMaps.clear();
      declarationMaps.set(raw, out);
      return out;
    }
    _write(map) {
      // Trailing semicolon, as a browser serialises it: `color: red;`. Pages do
      // compare `getAttribute("style")` against a literal.
      const text = [...map.entries()].map(([k, v]) => `${k}: ${v};`).join(" ");
      this._source.set(text);
    }
    get length() { return this._read().size; }
    item(index) { return [...this._read().keys()][index] ?? ""; }

    getPropertyValue(name) {
      const property = String(name).toLowerCase();
      return serializedValue(property, this._read().get(property));
    }
    setProperty(name, value) {
      // A **copy**: `_read()` hands back the shared memo, and mutating it would
      // corrupt the entry every other element with the same `style` text reads.
      const map = new Map(this._read());
      if (value === "" || value === null || value === undefined) {
        map.delete(String(name).toLowerCase());
      } else {
        map.set(String(name).toLowerCase(), String(value));
      }
      this._write(map);
    }
    removeProperty(name) {
      const property = String(name).toLowerCase();
      const map = new Map(this._read());
      // The **serialised** value, the same one `getPropertyValue` would have
      // answered a moment earlier. Returning the raw text made the two
      // disagree: `.5` from one and `0.5` from the other for one declaration.
      const had = serializedValue(property, map.get(property));
      map.delete(property);
      this._write(map);
      return had;
    }
    get cssText() { return this._source.get(); }
    set cssText(text) { this._source.set(String(text)); }
  }

  /// The backing an element's inline style uses: its own `style` attribute, so
  /// what script sets is what the cascade sees and what `getAttribute("style")`
  /// returns.
  function inlineStyleSource(node) {
    return {
      get: () => api.getAttr(node._id, "style") || "",
      // Always written, never removed: emptying a style declaration leaves an
      // empty `style` attribute in a browser, and `getAttribute("style")`
      // answers "" rather than null.
      set: (text) => api.setAttr(node._id, "style", text),
    };
  }

  // `el.style.backgroundColor = 'red'` has to reach `background-color`, so the
  // camelCase surface is a proxy over the dashed one rather than a second list
  // that could disagree with it.
  const styleHandler = {
    get(target, key) {
      if (typeof key !== "string" || key in target) return Reflect.get(target, key);
      return target.getPropertyValue(camelToDash(key));
    },
    // `"color" in el.style` is how pages feature-detect a CSS property, and
    // WPT cross-checks the answer against `CSS.supports` — so both are
    // answered by the same authority: Stylo's parser, asked with `inherit`
    // (valid for every real property). The vendor dance maps `WebkitFoo`
    // back to `-webkit-foo`, which camel-to-dash alone cannot know.
    has(target, key) {
      if (typeof key !== "string" || key in target) return Reflect.has(target, key);
      const dash = camelToDash(key);
      if (api.supportsCss(dash, "inherit")) return true;
      return /^(webkit|moz|ms|o)-/.test(dash) && api.supportsCss(`-${dash}`, "inherit");
    },
    set(target, key, value) {
      if (typeof key === "string" && !(key in target)) {
        target.setProperty(camelToDash(key), value);
        return true;
      }
      return Reflect.set(target, key, value);
    },
  };
  const RawStyleDeclaration = StyleDeclaration;
  StyleDeclaration = function (source) {
    return new Proxy(new RawStyleDeclaration(source), styleHandler);
  };

  // ── events ───────────────────────────────────────────────────────────────

  const listeners = [];

  class Event {
    constructor(type, init) {
      this.type = String(type);
      this.bubbles = !!(init && init.bubbles);
      this.cancelable = !!(init && init.cancelable);
      this.composed = !!(init && init.composed);
      this.defaultPrevented = false;
      this.target = null;
      this.currentTarget = null;
      this.eventPhase = 0;
      this.timeStamp = clock;
      this.isTrusted = false;
      this._stopped = false;
    }
    preventDefault() {
      // A passive listener's preventDefault is a no-op by definition — that
      // is the whole contract of passive.
      if (this.__h5iInPassive) return;
      if (this.cancelable !== false) this.defaultPrevented = true;
    }
    stopPropagation() { this._stopped = true; }
    stopImmediatePropagation() { this._stopped = true; }
    composedPath() { return path(this.target || null); }
  }

  // The concrete types a page actually constructs and reads fields off. A
  // single generic Event meant `event.detail` and `event.key` were undefined,
  // which is the kind of gap a framework notices immediately and silently.
  class CustomEvent extends Event {
    constructor(type, init) { super(type, init); this.detail = (init && init.detail) ?? null; }
  }
  class UIEvent extends Event {
    constructor(type, init) { super(type, init); this.detail = (init && init.detail) || 0; }
  }
  class MouseEvent extends UIEvent {
    constructor(type, init) {
      super(type, init);
      const i = init || {};
      this.clientX = i.clientX || 0; this.clientY = i.clientY || 0;
      this.screenX = i.screenX || 0; this.screenY = i.screenY || 0;
      this.pageX = i.pageX || this.clientX; this.pageY = i.pageY || this.clientY;
      this.button = i.button || 0; this.buttons = i.buttons || 0;
      this.altKey = !!i.altKey; this.ctrlKey = !!i.ctrlKey;
      this.shiftKey = !!i.shiftKey; this.metaKey = !!i.metaKey;
    }
  }
  class KeyboardEvent extends UIEvent {
    constructor(type, init) {
      super(type, init);
      const i = init || {};
      this.key = i.key || ""; this.code = i.code || "";
      this.repeat = !!i.repeat; this.isComposing = !!i.isComposing;
      this.altKey = !!i.altKey; this.ctrlKey = !!i.ctrlKey;
      this.shiftKey = !!i.shiftKey; this.metaKey = !!i.metaKey;
    }
  }
  class InputEvent extends UIEvent {
    constructor(type, init) {
      super(type, init);
      const i = init || {};
      this.data = i.data ?? null; this.inputType = i.inputType || "insertText";
    }
  }

  // `initEvent`, which is how pre-constructor code configures an event —
  // and what `document.createEvent` hands back is useless without it.
  Object.assign(Event.prototype, {
    initEvent(type, bubbles, cancelable) {
      this.type = String(type);
      this.bubbles = !!bubbles;
      this.cancelable = !!cancelable;
    },
  });
  Object.assign(CustomEvent.prototype, {
    initCustomEvent(type, bubbles, cancelable, detail) {
      Event.prototype.initEvent.call(this, type, bubbles, cancelable);
      this.detail = detail ?? null;
    },
  });
  Object.assign(UIEvent.prototype, {
    initUIEvent(type, bubbles, cancelable, view, detail) {
      Event.prototype.initEvent.call(this, type, bubbles, cancelable);
      void view;
      this.detail = detail || 0;
    },
  });
  Object.assign(MouseEvent.prototype, {
    initMouseEvent(type, bubbles, cancelable, view, detail, sx, sy, cx, cy,
      ctrl, alt, shift, meta, button, related) {
      UIEvent.prototype.initUIEvent.call(this, type, bubbles, cancelable, view, detail);
      this.screenX = sx || 0; this.screenY = sy || 0;
      this.clientX = cx || 0; this.clientY = cy || 0;
      this.ctrlKey = !!ctrl; this.altKey = !!alt;
      this.shiftKey = !!shift; this.metaKey = !!meta;
      this.button = button || 0; this.relatedTarget = related ?? null;
    },
  });
  Object.assign(KeyboardEvent.prototype, {
    initKeyboardEvent(type, bubbles, cancelable, view, key) {
      Event.prototype.initEvent.call(this, type, bubbles, cancelable);
      void view;
      if (key !== undefined) this.key = String(key);
    },
  });

  // The rest of the event types a page constructs by name.
  //
  // Each of these was a `ReferenceError` — `new SubmitEvent("submit", …)` did
  // not merely lose a field, it threw and took the handler with it. They are
  // cheap because an event *is* its fields: the dispatch machinery is shared,
  // and what distinguishes a `StorageEvent` from an `ErrorEvent` is which
  // properties it carries. Adding the ones a page actually constructs is the
  // difference between a listener that reads `event.submitter` and a page that
  // stopped at the constructor.
  class FocusEvent extends UIEvent {
    constructor(type, init) { super(type, init); this.relatedTarget = (init && init.relatedTarget) ?? null; }
  }
  class WheelEvent extends MouseEvent {
    constructor(type, init) {
      super(type, init);
      const i = init || {};
      this.deltaX = i.deltaX || 0; this.deltaY = i.deltaY || 0;
      this.deltaZ = i.deltaZ || 0; this.deltaMode = i.deltaMode || 0;
    }
  }
  class PointerEvent extends MouseEvent {
    constructor(type, init) {
      super(type, init);
      const i = init || {};
      this.pointerId = i.pointerId || 0;
      this.pointerType = i.pointerType || "";
      this.isPrimary = !!i.isPrimary;
      this.pressure = i.pressure || 0;
      this.width = i.width === undefined ? 1 : i.width;
      this.height = i.height === undefined ? 1 : i.height;
    }
  }
  class CompositionEvent extends UIEvent {
    constructor(type, init) { super(type, init); this.data = (init && init.data) || ""; }
  }
  class ErrorEvent extends Event {
    constructor(type, init) {
      super(type, init);
      const i = init || {};
      this.message = i.message || ""; this.filename = i.filename || "";
      this.lineno = i.lineno || 0; this.colno = i.colno || 0;
      this.error = i.error ?? null;
    }
  }
  class PromiseRejectionEvent extends Event {
    constructor(type, init) {
      super(type, init);
      const i = init || {};
      this.promise = i.promise ?? null; this.reason = i.reason;
    }
  }
  class ProgressEvent extends Event {
    constructor(type, init) {
      super(type, init);
      const i = init || {};
      this.lengthComputable = !!i.lengthComputable;
      this.loaded = i.loaded || 0; this.total = i.total || 0;
    }
  }
  class MessageEvent extends Event {
    constructor(type, init) {
      super(type, init);
      const i = init || {};
      this.data = i.data ?? null; this.origin = i.origin || "";
      this.lastEventId = i.lastEventId || ""; this.source = i.source ?? null;
      this.ports = i.ports || [];
    }
  }
  class CloseEvent extends Event {
    constructor(type, init) {
      super(type, init);
      const i = init || {};
      this.wasClean = !!i.wasClean; this.code = i.code || 0; this.reason = i.reason || "";
    }
  }
  class StorageEvent extends Event {
    constructor(type, init) {
      super(type, init);
      const i = init || {};
      this.key = i.key ?? null; this.oldValue = i.oldValue ?? null;
      this.newValue = i.newValue ?? null; this.url = i.url || "";
      this.storageArea = i.storageArea ?? null;
    }
  }
  class PopStateEvent extends Event {
    constructor(type, init) { super(type, init); this.state = (init && init.state) ?? null; }
  }
  class HashChangeEvent extends Event {
    constructor(type, init) {
      super(type, init);
      const i = init || {};
      this.oldURL = i.oldURL || ""; this.newURL = i.newURL || "";
    }
  }
  class PageTransitionEvent extends Event {
    constructor(type, init) { super(type, init); this.persisted = !!(init && init.persisted); }
  }
  class SubmitEvent extends Event {
    constructor(type, init) { super(type, init); this.submitter = (init && init.submitter) ?? null; }
  }
  class FormDataEvent extends Event {
    constructor(type, init) { super(type, init); this.formData = (init && init.formData) ?? null; }
  }
  class ToggleEvent extends Event {
    constructor(type, init) {
      super(type, init);
      const i = init || {};
      this.oldState = i.oldState || ""; this.newState = i.newState || "";
    }
  }
  // The Invoker Commands half of what ToggleEvent is to popovers: fired at the
  // element a `<button commandfor command>` points at, carrying which command
  // and which button, so one listener can serve many invokers.
  class CommandEvent extends Event {
    constructor(type, init) {
      super(type, init);
      const i = init || {};
      this.command = i.command !== undefined ? String(i.command) : "";
      this.source = i.source ?? null;
    }
  }
  class AnimationEvent extends Event {
    constructor(type, init) {
      super(type, init);
      const i = init || {};
      this.animationName = i.animationName || ""; this.elapsedTime = i.elapsedTime || 0;
      this.pseudoElement = i.pseudoElement || "";
    }
  }
  // The interfaces that exist mostly *because* `document.createEvent` names
  // them. Nobody constructs a DeviceMotionEvent by hand in 2026, but the
  // createEvent alias table is spec text and every row of it is tested.
  class BeforeUnloadEvent extends Event {
    constructor(type, init) { super(type, init); this.returnValue = ""; }
  }
  class DragEvent extends MouseEvent {
    constructor(type, init) { super(type, init); this.dataTransfer = (init && init.dataTransfer) ?? null; }
  }
  class TextEvent extends UIEvent {
    constructor(type, init) { super(type, init); this.data = (init && init.data) || ""; }
    initTextEvent(type, bubbles, cancelable, view, data) {
      Event.prototype.initEvent.call(this, type, bubbles, cancelable);
      void view;
      this.data = data === undefined ? "" : String(data);
    }
  }
  class DeviceMotionEvent extends Event {
    constructor(type, init) {
      super(type, init);
      const i = init || {};
      this.acceleration = i.acceleration ?? null;
      this.accelerationIncludingGravity = i.accelerationIncludingGravity ?? null;
      this.rotationRate = i.rotationRate ?? null;
      this.interval = i.interval ?? 0;
    }
  }
  class DeviceOrientationEvent extends Event {
    constructor(type, init) {
      super(type, init);
      const i = init || {};
      this.alpha = i.alpha ?? null; this.beta = i.beta ?? null;
      this.gamma = i.gamma ?? null; this.absolute = !!i.absolute;
    }
  }

  class TransitionEvent extends Event {
    constructor(type, init) {
      super(type, init);
      const i = init || {};
      this.propertyName = i.propertyName || ""; this.elapsedTime = i.elapsedTime || 0;
      this.pseudoElement = i.pseudoElement || "";
    }
  }

  // The event classes above initialise their fields as own properties, which
  // is right for instances and invisible to anyone inspecting the *interface*:
  // idlharness asks the prototype for each attribute. These accessors are that
  // declaration — the getter answers the field's default (an instance's own
  // property shadows it), and the setter exists so the constructors' strict-
  // mode assignments still land as own data properties instead of throwing at
  // a getter-only accessor.
  {
    const declareEventFields = (Interface, fields) => {
      for (const [field, fallback] of Object.entries(fields)) {
        const getter = function () { return fallback; };
        const setter = function (value) {
          Object.defineProperty(this, field, {
            value, writable: true, enumerable: true, configurable: true,
          });
        };
        Object.defineProperty(getter, "name", { value: `get ${field}` });
        Object.defineProperty(setter, "name", { value: `set ${field}` });
        Object.defineProperty(Interface.prototype, field, {
          configurable: true, enumerable: true, get: getter, set: setter,
        });
      }
    };
    declareEventFields(ToggleEvent, { oldState: "", newState: "", source: null });
    declareEventFields(CommandEvent, { command: "", source: null });
    declareEventFields(SubmitEvent, { submitter: null });
    declareEventFields(FormDataEvent, { formData: null });
    declareEventFields(PageTransitionEvent, { persisted: false });
    declareEventFields(ErrorEvent, {
      message: "", filename: "", lineno: 0, colno: 0, error: undefined,
    });
    declareEventFields(MessageEvent, {
      data: null, origin: "", lastEventId: "", source: null, ports: [],
    });
    declareEventFields(StorageEvent, {
      key: null, oldValue: null, newValue: null, url: "", storageArea: null,
    });
    declareEventFields(PopStateEvent, { state: null, hasUAVisualTransition: false });
    declareEventFields(HashChangeEvent, { oldURL: "", newURL: "" });
    declareEventFields(PromiseRejectionEvent, { promise: undefined, reason: undefined });
    declareEventFields(AnimationEvent, {
      animationName: "", elapsedTime: 0, pseudoElement: "",
    });
    declareEventFields(TransitionEvent, {
      propertyName: "", elapsedTime: 0, pseudoElement: "",
    });
  }

  function path(node) {
    const chain = [];
    for (let n = node; n; n = n.parentNode) chain.push(n);
    return chain;
  }

  // Capture down, then bubble up: the order a page's handlers were written
  // against. A listener that throws does not stop the others, because one bad
  // handler taking the page down is worse than one handler not running.
  function dispatch(target, event) {
    event.target = target;
    const chain = path(target);

    const fire = (node, capture) => {
      if (event._stopped) return;
      event.currentTarget = node;
      for (const l of listeners.slice()) {
        if (l.id !== node._id || l.type !== event.type || l.capture !== capture) continue;
        // Removed *before* the call, not after: a handler that throws, or that
        // dispatches the same event again, must still not run twice. `once`
        // was being ignored entirely, so a page relying on it double-handled
        // and nothing said so.
        if (l.once) {
          const at = listeners.indexOf(l);
          if (at >= 0) listeners.splice(at, 1);
        }
        try {
          if (l.passive) event.__h5iInPassive = true;
          if (typeof l.handler === "function") l.handler.call(node, event);
          else if (l.handler && typeof l.handler.handleEvent === "function") {
            l.handler.handleEvent(event);
          }
        } catch (error) {
          console.error("listener for " + event.type + " threw: " + withStack(error));
        } finally {
          event.__h5iInPassive = false;
        }
      }
    };

    for (let i = chain.length - 1; i >= 1; i--) fire(chain[i], true);
    fire(target, true);
    fire(target, false);
    if (event.bubbles) for (let i = 1; i < chain.length; i++) fire(chain[i], false);

    return !event.defaultPrevented;
  }

  // An API this engine does not implement, made to say so.

  // Real, because the engine already contains a correct URL parser and a
  // second one written in JavaScript would disagree with it about exactly the
  // cases that matter.
  class URLSearchParams {
    constructor(init) {
      this._pairs = [];
      if (typeof init === "string") {
        for (const part of init.replace(/^\?/, "").split("&")) {
          if (!part) continue;
          const at = part.indexOf("=");
          const k = at < 0 ? part : part.slice(0, at);
          const v = at < 0 ? "" : part.slice(at + 1);
          this._pairs.push([decodeURIComponent(k.replace(/\+/g, " ")),
                            decodeURIComponent(v.replace(/\+/g, " "))]);
        }
      } else if (init && typeof init === "object") {
        // Three shapes, and only the third was handled. `new
        // URLSearchParams(otherParams)` walked the *object's own keys*, so it
        // copied the internal `_pairs` field and produced `_pairs=a%2Cb` —
        // a params object serialising its own implementation.
        if (typeof init[Symbol.iterator] === "function") {
          // A sequence of pairs, which covers another URLSearchParams, a Map,
          // and the `[["a","b"]]` literal form.
          for (const pair of init) {
            const entry = Array.from(pair);
            if (entry.length !== 2) {
              throw new TypeError(
                "URLSearchParams: each entry must have exactly two elements",
              );
            }
            this._pairs.push([String(entry[0]), String(entry[1])]);
          }
        } else {
          for (const k of Object.keys(init)) this._pairs.push([k, String(init[k])]);
        }
      }
    }
    get(k) { const hit = this._pairs.find(([n]) => n === String(k)); return hit ? hit[1] : null; }
    getAll(k) { return this._pairs.filter(([n]) => n === String(k)).map(([, v]) => v); }
    has(k) { return this._pairs.some(([n]) => n === String(k)); }
    set(k, v) { this.delete(k); this.append(k, v); }
    append(k, v) { this._pairs.push([String(k), String(v)]); }
    delete(k) { this._pairs = this._pairs.filter(([n]) => n !== String(k)); }
    forEach(fn) { for (const [k, v] of this._pairs) fn(v, k, this); }
    get size() { return this._pairs.length; }
    // Stable, and by *code unit* rather than by `<` on strings, which is what
    // the spec says and what makes the order the same in every engine.
    sort() {
      this._pairs = this._pairs
        .map((pair, index) => [pair, index])
        .sort(([a, i], [b, j]) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : i - j))
        .map(([pair]) => pair);
    }
    keys() { return this._pairs.map(([k]) => k)[Symbol.iterator](); }
    values() { return this._pairs.map(([, v]) => v)[Symbol.iterator](); }
    entries() { return this._pairs[Symbol.iterator](); }
    [Symbol.iterator]() { return this.entries(); }
    toString() {
      // `application/x-www-form-urlencoded` serialisation, which is not
      // `encodeURIComponent`: a space is `+`, and `!'()~*` are escaped where
      // `encodeURIComponent` leaves them alone.
      const encode = (text) =>
        encodeURIComponent(text)
          .replace(/%20/g, "+")
          .replace(/[!'()~*]/g, (c) => "%" + c.charCodeAt(0).toString(16).toUpperCase());
      return this._pairs.map(([k, v]) => encode(k) + "=" + encode(v)).join("&");
    }
  }

  class URL {
    constructor(href, base) {
      const parts = api.parseUrl(String(href), base === undefined ? "" : String(base));
      if (!parts) throw new TypeError(`Invalid URL: ${href}`);
      this._parts = parts;
      this.searchParams = new URLSearchParams(parts.search);
    }
    /// Every component setter follows the same shape the URL Standard gives
    /// them: strip tab and newline (the parser eats those anywhere), rebuild
    /// the serialisation with the one component swapped, and re-parse — a
    /// candidate the parser refuses leaves the URL exactly as it was, which
    /// is why `url.protocol = "\0https"` changes nothing instead of storing
    /// garbage.
    _serializeWith(overrides) {
      const p = this._parts;
      const protocol = overrides.protocol ?? p.protocol;
      const host = overrides.host ?? p.host;
      const pathname = overrides.pathname ?? p.pathname;
      const search = overrides.search ?? p.search;
      const hash = overrides.hash ?? p.hash;
      if (!p.host && !p.href.startsWith(`${p.protocol}//`)) {
        // An opaque path (`data:`, `mailto:`) has no authority to edit.
        return `${protocol}${pathname}${search}${hash}`;
      }
      // Userinfo is carried through. It used to be dropped here, which was
      // invisible only because `username` and `password` were hard-coded to "";
      // with them readable, `url.host = "x"` on `https://u:p@a/` would have
      // quietly deleted the credentials.
      const user = p.username ?? "";
      const secret = p.password ?? "";
      const userinfo = user || secret
        ? `${user}${secret ? `:${secret}` : ""}@`
        : "";
      return `${protocol}//${userinfo}${host}${pathname}${search}${hash}`;
    }
    _tryAdopt(candidate) {
      const p = api.parseUrl(String(candidate), "");
      if (!p) return;
      this._parts = p;
      if (this.searchParams) {
        this.searchParams._pairs = new URLSearchParams(p.search)._pairs;
      }
    }
    get href() { return this._parts.href; }
    set href(value) {
      const p = api.parseUrl(String(value), "");
      if (!p) throw new TypeError(`Invalid URL: ${value}`);
      this._parts = p;
      this.searchParams._pairs = new URLSearchParams(p.search)._pairs;
    }
    get protocol() { return this._parts.protocol; }
    set protocol(value) {
      const cleaned = String(value).replace(/[\t\n\r]/g, "");
      const m = /^[A-Za-z][A-Za-z0-9+.\-]*/.exec(cleaned);
      if (!m) return;
      const rest = this._parts.href.slice(this._parts.protocol.length);
      this._tryAdopt(`${m[0]}:${rest}`);
    }
    get host() { return this._parts.host; }
    set host(value) {
      this._tryAdopt(this._serializeWith({ host: String(value).replace(/[\t\n\r]/g, "") }));
    }
    get hostname() { return this._parts.hostname; }
    set hostname(value) {
      const cleaned = String(value).replace(/[\t\n\r]/g, "");
      const port = this._parts.port;
      this._tryAdopt(this._serializeWith({ host: port ? `${cleaned}:${port}` : cleaned }));
    }
    get port() { return this._parts.port; }
    set port(value) {
      const digits = /^\d*/.exec(String(value).replace(/[\t\n\r]/g, ""))[0];
      const host = digits === ""
        ? this._parts.hostname
        : `${this._parts.hostname}:${digits}`;
      this._tryAdopt(this._serializeWith({ host }));
    }
    get pathname() { return this._parts.pathname; }
    set pathname(value) {
      const cleaned = String(value).replace(/[\t\n\r]/g, "");
      this._tryAdopt(this._serializeWith({
        pathname: cleaned.startsWith("/") ? cleaned : `/${cleaned}`,
      }));
    }
    get search() { return this._parts.search; }
    set search(value) {
      const cleaned = String(value).replace(/[\t\n\r]/g, "");
      const search = cleaned === "" ? "" : (cleaned.startsWith("?") ? cleaned : `?${cleaned}`);
      this._tryAdopt(this._serializeWith({ search }));
    }
    get hash() { return this._parts.hash; }
    set hash(value) {
      const cleaned = String(value).replace(/[\t\n\r]/g, "");
      const hash = cleaned === "" ? "" : (cleaned.startsWith("#") ? cleaned : `#${cleaned}`);
      this._tryAdopt(this._serializeWith({ hash }));
    }
    get origin() { return this._parts.origin; }
    /// The userinfo half.
    ///
    /// Both were hard-coded to "" with a note saying the parser did not surface
    /// them. It always had: `url::Url` carries a username and a password, and
    /// reading them was one line each. The setters go back through the parser
    /// rather than rebuilding the href here, because the URL Standard
    /// percent-encodes userinfo with its own set and a raw control character in
    /// an authority is a parse failure — `url.username = "\0test"` has to
    /// store `%00test`, which a re-parse of a hand-built string cannot do.
    get username() { return this._parts.username ?? ""; }
    set username(value) {
      const href = api.urlWithUserinfo(this._parts.href, "username", String(value));
      if (href !== null) this._tryAdopt(href);
    }
    get password() { return this._parts.password ?? ""; }
    set password(value) {
      const href = api.urlWithUserinfo(this._parts.href, "password", String(value));
      if (href !== null) this._tryAdopt(href);
    }
    toString() { return this.href; }
    toJSON() { return this.href; }
    /// The two statics, which are the non-throwing way to ask. A page testing
    /// a URL had to build one in a `try`, which is the idiom these exist to
    /// replace.
    static parse(href, base) {
      try {
        return new URL(href, base);
      } catch {
        return null;
      }
    }
    static canParse(href, base) {
      return URL.parse(href, base) !== null;
    }
  }

  // A case-insensitive header map, which is what `Headers` is: `get("ETag")`
  // must find a header the server spelled `etag`.
  class Headers {
    constructor(init) {
      this._map = new Map();
      if (init instanceof Headers) for (const [k, v] of init._map) this._map.set(k, v);
      else if (Array.isArray(init)) for (const [k, v] of init) this.append(k, v);
      else if (init && typeof init === "object") {
        for (const k of Object.keys(init)) this.set(k, init[k]);
      }
    }
    get(name) { const v = this._map.get(String(name).toLowerCase()); return v === undefined ? null : v; }
    set(name, value) { this._map.set(String(name).toLowerCase(), String(value)); }
    has(name) { return this._map.has(String(name).toLowerCase()); }
    delete(name) { this._map.delete(String(name).toLowerCase()); }
    append(name, value) {
      const key = String(name).toLowerCase();
      const existing = this._map.get(key);
      this._map.set(key, existing === undefined ? String(value) : existing + ", " + value);
    }
    forEach(fn) { for (const [k, v] of this._map) fn(v, k, this); }
    keys() { return this._map.keys(); }
    values() { return this._map.values(); }
    entries() { return this._map.entries(); }
    [Symbol.iterator]() { return this._map.entries(); }
  }

  class Request {
    constructor(input, init) {
      this.url = typeof input === "string" ? input : String(input && input.url);
      const i = init || {};
      this.method = (i.method || "GET").toUpperCase();
      this.headers = new Headers(i.headers);
      this.body = i.body ?? null;
      // Every Request carries a signal, minted here when the caller brought
      // none — that is the spec's shape, and `request.signal` being null sent
      // every page that wires its own abort through the request object down
      // the wrong branch.
      this.signal = i.signal || (input instanceof Request ? input.signal : null) || new AbortSignal();
      // The two that decide what happens at an origin boundary. Defaults are
      // the spec's — `cors` and `same-origin` — so a page that says nothing
      // gets what a browser gives it: the request may cross, and it does not
      // take the session with it.
      this.mode = i.mode || (input instanceof Request ? input.mode : "cors");
      this.credentials =
        i.credentials || (input instanceof Request ? input.credentials : "same-origin");
      this.bodyUsed = false;
    }
    // A request carries a body too, and reading it back is how a service
    // worker or a test inspects one. Same readers as `Response`, over the same
    // kind of value.
    _text() { return this.body == null ? "" : String(this.body); }
    text() { this.bodyUsed = true; return Promise.resolve(this._text()); }
    json() { this.bodyUsed = true; return Promise.resolve(JSON.parse(this._text())); }
    formData() {
      this.bodyUsed = true;
      const form = new FormData();
      for (const [k, v] of new URLSearchParams(this._text())) form.append(k, v);
      return Promise.resolve(form);
    }
    arrayBuffer() {
      this.bodyUsed = true;
      return Promise.resolve(new TextEncoder().encode(this._text()).buffer);
    }
    blob() {
      this.bodyUsed = true;
      return Promise.resolve(new Blob([this._text()],
        { type: this.headers.get("content-type") || "" }));
    }
    clone() { return new Request(this.url, this); }
  }

  // What `fetch` resolves to, and what a page constructs to mock one.
  //
  // It was a plain object literal built inside `responseFrom`, which worked for
  // every page that only reads it and failed for the two that ask *what it is*:
  // `Response` was not a global, so `new Response(...)` was a ReferenceError and
  // `res instanceof Response` could not be written at all. Both are ordinary
  // things for a page to do, and a mocked fetch does the first.
  class Response {
    constructor(body, init) {
      const i = init || {};
      this._body = body == null ? "" : String(body);
      this.status = i.status === undefined ? 200 : Number(i.status);
      this.statusText = i.statusText === undefined ? "" : String(i.statusText);
      this.ok = this.status >= 200 && this.status < 300;
      this.headers = i.headers instanceof Headers ? i.headers : new Headers(i.headers);
      this.type = i.type || "basic";
      this.url = i.url || "";
      this.redirected = !!i.redirected;
      this.bodyUsed = false;
    }
    text() { this.bodyUsed = true; return Promise.resolve(this._body); }
    json() { this.bodyUsed = true; return Promise.resolve(JSON.parse(this._body)); }
    // The other two body readers. `formData` is how anything that posts a form
    // reads one back, and it is what `url/urlencoded-parser` tests against.
    formData() {
      this.bodyUsed = true;
      const form = new FormData();
      for (const [k, v] of new URLSearchParams(this._body)) form.append(k, v);
      return Promise.resolve(form);
    }
    arrayBuffer() {
      this.bodyUsed = true;
      const text = this._body;
      const bytes = new TextEncoder().encode(text);
      return Promise.resolve(bytes.buffer);
    }
    blob() {
      this.bodyUsed = true;
      const type = this.headers.get("content-type") || "";
      return Promise.resolve(new Blob([this._body], { type }));
    }
    clone() {
      return new Response(this._body, {
        status: this.status, statusText: this.statusText, headers: this.headers,
        type: this.type, url: this.url, redirected: this.redirected,
      });
    }
    static json(data, init) {
      const response = new Response(JSON.stringify(data), init);
      response.headers.set("content-type", "application/json");
      return response;
    }
    static error() { return new Response("", { status: 0, type: "error" }); }
  }

  // Real enough to be useful: a fetch already aborted is refused, and abort
  // fires its listeners. It cannot cancel a request in flight, because this
  // engine's fetch is synchronous underneath — that limit is stated rather
  // than papered over with a promise that never settles.
  /// The default abort reason, which is not `new Error`.
  ///
  /// Every consumer that distinguishes an abort from a failure does it by
  /// `e.name === "AbortError"` — that is the documented idiom, and rejecting
  /// with a plain Error made every such branch take the failure path.
  function abortError() {
    return new DOMException("The operation was aborted.", "AbortError");
  }

  class AbortSignal {
    constructor() { this.aborted = false; this.reason = undefined; this._listeners = []; }
    addEventListener(type, handler) { if (type === "abort") this._listeners.push(handler); }
    removeEventListener(type, handler) {
      if (type !== "abort") return;
      const at = this._listeners.indexOf(handler);
      if (at >= 0) this._listeners.splice(at, 1);
    }
    throwIfAborted() { if (this.aborted) throw this.reason; }
    /// Deliver the abort: flip the state, then tell every listener.
    ///
    /// On the signal rather than the controller, because three producers need
    /// it now — the controller, `AbortSignal.timeout`, and `AbortSignal.any` —
    /// and the delivery has to be identical from all three or a listener's
    /// behaviour depends on who aborted it.
    _fire(reason) {
      if (this.aborted) return;
      this.aborted = true;
      this.reason = reason === undefined ? abortError() : reason;
      const event = new Event("abort", { bubbles: false });
      for (const handler of this._listeners.slice()) {
        try { handler.call(this, event); } catch (e) { console.error("abort listener threw: " + e); }
      }
      if (typeof this.onabort === "function") this.onabort(event);
    }
    static abort(reason) {
      const s = new AbortSignal();
      s.aborted = true;
      s.reason = reason === undefined ? abortError() : reason;
      return s;
    }
    /// A signal that aborts itself after `ms`, with the *timeout* name.
    ///
    /// On this engine's virtual clock, which is the interesting part: a page
    /// racing a fetch against `AbortSignal.timeout(5000)` settles
    /// deterministically here, where a wall-clock engine gives a different
    /// answer under load.
    static timeout(ms) {
      const s = new AbortSignal();
      setTimeout(() => s._fire(new DOMException("The operation timed out.", "TimeoutError")), ms);
      return s;
    }
    /// Aborted when any input is. Already-aborted inputs win immediately, and
    /// the reason is the *first* input's, both per spec.
    static any(signals) {
      const s = new AbortSignal();
      for (const input of signals) {
        if (input.aborted) { s.aborted = true; s.reason = input.reason; return s; }
      }
      for (const input of signals) {
        input.addEventListener("abort", () => s._fire(input.reason));
      }
      return s;
    }
  }

  class AbortController {
    constructor() { this.signal = new AbortSignal(); }
    abort(reason) { this.signal._fire(reason); }
  }

  /// Build a form's *entry list*, which is the algorithm submission is made of.
  function buildEntryList(form, submitter) {
    const entries = [];
    for (const field of form.elements) {
      const tag = field.tagName;
      if (tag === "FIELDSET" || tag === "OUTPUT") continue;
      if (field.disabled) continue;
      // A control inside a `<datalist>` is a suggestion, never an entry.
      let inDatalist = false;
      for (let n = field.parentNode; n; n = n.parentNode) {
        if (n.tagName === "DATALIST") { inDatalist = true; break; }
      }
      if (inDatalist) continue;

      const type = tag === "INPUT"
        ? (api.getAttr(field._id, "type") || "text").toLowerCase()
        : "";
      // Buttons are entries only when they are the one that submitted.
      if (tag === "BUTTON" || type === "submit" || type === "reset"
        || type === "button" || type === "image") {
        if (!submitter || field._id !== submitter._id) continue;
        if (type === "reset" || type === "button"
          || (tag === "BUTTON" && (api.getAttr(field._id, "type") || "submit").toLowerCase() !== "submit")) {
          continue;
        }
      }
      const name = field.name;
      if (!name) continue;
      if ((type === "checkbox" || type === "radio") && !field.checked) continue;
      if (type === "file") {
        // No file can have been chosen here, and the spec's own answer for an
        // empty file control is an entry with an empty filename rather than no
        // entry at all — a server counting fields would otherwise see one
        // fewer than the form has.
        entries.push([name, ""]);
        continue;
      }
      // The one field whose value the *browser* supplies.
      if (type === "hidden" && name.toLowerCase() === "_charset_") {
        entries.push([name, "UTF-8"]);
        continue;
      }
      entries.push([name, field.value]);
    }
    return entries;
  }

  /// Submit a form for real: hand its entry list to the engine.
  ///
  /// The list is built here, where the algorithm lives; the encoding is the
  /// engine's, so a form and `websec replay` cannot disagree. `FormData` rather
  /// than `buildEntryList` because it fires `formdata`.
  function submitTheForm(form, submitter) {
    const attribute = (name) => {
      const override = submitter && api.getAttr(submitter._id, "form" + name);
      return override != null ? override : (api.getAttr(form._id, name) ?? "");
    };
    const data = new FormData(form, submitter ?? null);
    form.__h5iEntryList = data._entries;
    // `action` reflects the document's address when the attribute is missing,
    // which is where an actionless form submits.
    const action = (submitter && api.getAttr(submitter._id, "formaction") != null)
      ? submitter.formAction
      : form.action;
    api.submitForm(
      String(action || ""),
      attribute("method"),
      attribute("enctype"),
      data._entries.map(([name, value]) => [String(name), String(value)]),
    );
  }

  class FormData {
    constructor(form, submitter) {
      this._entries = [];
      if (form) {
        if (submitter !== undefined && submitter !== null) {
          const owner = submitter.form;
          if (!owner || !form || owner._id !== form._id) {
            throw new DOMException(
              "FormData: the submitter is not owned by this form",
              "NotFoundError",
            );
          }
        }
        this._entries = buildEntryList(form, submitter ?? null);
        // `formdata` fires with this object, so a listener adding entries adds
        // them to the list being constructed rather than to a copy.
        form.dispatchEvent(new FormDataEvent("formdata", {
          bubbles: true, formData: this,
        }));
      }
    }
    forEach(fn, thisArg) {
      for (const [k, v] of this._entries.slice()) fn.call(thisArg, v, k, this);
    }
    append(k, v) { this._entries.push([String(k), String(v)]); }
    set(k, v) {
      this.delete(k);
      this.append(k, v);
    }
    get(k) { const hit = this._entries.find(([n]) => n === String(k)); return hit ? hit[1] : null; }
    getAll(k) { return this._entries.filter(([n]) => n === String(k)).map(([, v]) => v); }
    has(k) { return this._entries.some(([n]) => n === String(k)); }
    delete(k) { this._entries = this._entries.filter(([n]) => n !== String(k)); }
    entries() { return this._entries[Symbol.iterator](); }
    keys() { return this._entries.map(([n]) => n)[Symbol.iterator](); }
    values() { return this._entries.map(([, v]) => v)[Symbol.iterator](); }
    [Symbol.iterator]() { return this.entries(); }
    toString() {
      return this._entries
        .map(([k, v]) => encodeURIComponent(k) + "=" + encodeURIComponent(v))
        .join("&");
    }
  }

  // A media query, answered from the viewport the engine actually renders at.
  //
  // Returning `false` to everything — which is what a stub does — is not
  // neutral: a responsive layout asks `(min-width: …)` and then commits to the
  // branch it was told, so a wrong answer is a wrong page rather than a missing
  // feature. The features below have real answers here; anything else records
  // itself and reports no match, so the gap is visible instead of guessed at.
  /// What `matchMedia` hands back.
  class MediaQueryList extends EventTarget {
    constructor(text) {
      super();
      refuseExternal("MediaQueryList");
      this._media = String(text ?? "");
      this._matches = evaluateQuery(this._media, api.viewport());
      this.onchange = null;
    }
    get media() { return this._media; }
    get matches() { return this._matches; }
    addListener(fn) { this.addEventListener("change", fn); }
    removeListener(fn) { this.removeEventListener("change", fn); }
  }

  function matchMedia(query) {
    return internal(() => new MediaQueryList(String(query || "")));
  }

  function evaluateQuery(text, view) {
    const clauses = text.split(",").map((c) => c.trim()).filter(Boolean);
    if (clauses.length === 0) return false;
    // A comma-separated list is a disjunction, and `and` within a clause is a
    // conjunction. That is the whole grammar a page in practice uses.
    return clauses.some((clause) =>
      clause
        .split(/\band\b/)
        .map((part) => part.trim())
        .every((part) => evaluateFeature(part, view))
    );
  }

  function evaluateFeature(part, view) {
    const bare = part.replace(/^\(|\)$/g, "").trim().toLowerCase();
    if (!bare) return false;
    if (bare === "all" || bare === "screen") return true;
    if (bare === "print" || bare === "speech") return false;

    const at = bare.indexOf(":");
    if (at < 0) {
      api.unsupported(`matchMedia(${bare})`);
      return false;
    }
    const name = bare.slice(0, at).trim();
    const value = bare.slice(at + 1).trim();
    const px = (v) => parseFloat(v.replace(/px$/, ""));

    switch (name) {
      case "min-width": return view.width >= px(value);
      case "max-width": return view.width <= px(value);
      case "width": return view.width === px(value);
      case "min-height": return view.height >= px(value);
      case "max-height": return view.height <= px(value);
      case "height": return view.height === px(value);
      case "orientation": return value === (view.width >= view.height ? "landscape" : "portrait");
      case "prefers-color-scheme": return value === view.colorScheme;
      // Nothing animates here and there is no pointer, so these are not
      // guesses — they are what this engine is.
      case "prefers-reduced-motion": return value === "reduce";
      case "hover": return value === "none";
      case "any-hover": return value === "none";
      case "pointer": return value === "none";
      case "any-pointer": return value === "none";
      default:
        api.unsupported(`matchMedia(${name})`);
        return false;
    }
  }

  // ── layout observers ─────────────────────────────────────────────────────
  //
  // Both are driven from the settle loop rather than from a frame clock: this
  // engine has no frames at rest, and an observer that only fired on a repaint
  // would never fire at all. Checked after layout has been resolved, so the
  // rectangles they report are the ones that were actually laid out.

  const intersectionObservers = [];
  const resizeObservers = [];

  class IntersectionObserver {
    constructor(callback, options) {
      this._callback = callback;
      this._targets = [];
      this._seen = new Map();
      const raw = (options && options.threshold) ?? 0;
      this._thresholds = Array.isArray(raw) ? raw.slice().sort() : [raw];
      this.root = (options && options.root) || null;
      this.rootMargin = (options && options.rootMargin) || "0px";
    }
    observe(target) {
      if (!this._targets.includes(target)) this._targets.push(target);
      if (!intersectionObservers.includes(this)) intersectionObservers.push(this);
    }
    unobserve(target) {
      const at = this._targets.indexOf(target);
      if (at >= 0) this._targets.splice(at, 1);
    }
    disconnect() {
      this._targets.length = 0;
      const at = intersectionObservers.indexOf(this);
      if (at >= 0) intersectionObservers.splice(at, 1);
    }
    takeRecords() { return []; }

    _check(view) {
      const entries = [];
      for (const target of this._targets) {
        const [x, y, width, height] = api.rect(target._id) || [0, 0, 0, 0];
        const visibleW = Math.max(0, Math.min(x + width, view.width) - Math.max(x, 0));
        const visibleH = Math.max(0, Math.min(y + height, view.height) - Math.max(y, 0));
        const area = width * height;
        const ratio = area > 0 ? (visibleW * visibleH) / area : 0;
        const isIntersecting = this._thresholds.some(
          (t) => (t === 0 ? ratio > 0 : ratio >= t)
        );
        // Edges only: a page that lazy-loads on entry should be told once, not
        // on every settle for as long as the element stays on screen.
        if (this._seen.get(target._id) === isIntersecting) continue;
        this._seen.set(target._id, isIntersecting);
        entries.push({
          target, isIntersecting, intersectionRatio: ratio,
          boundingClientRect: target.getBoundingClientRect(),
          intersectionRect: { x, y, width: visibleW, height: visibleH,
                              top: y, left: x, right: x + visibleW, bottom: y + visibleH },
          rootBounds: { x: 0, y: 0, width: view.width, height: view.height,
                        top: 0, left: 0, right: view.width, bottom: view.height },
          time: clock,
        });
      }
      if (entries.length) deliverTo(this, entries);
    }
  }

  class ResizeObserver {
    constructor(callback) { this._callback = callback; this._targets = []; this._seen = new Map(); }
    observe(target) {
      if (!this._targets.includes(target)) this._targets.push(target);
      if (!resizeObservers.includes(this)) resizeObservers.push(this);
    }
    unobserve(target) {
      const at = this._targets.indexOf(target);
      if (at >= 0) this._targets.splice(at, 1);
    }
    disconnect() {
      this._targets.length = 0;
      const at = resizeObservers.indexOf(this);
      if (at >= 0) resizeObservers.splice(at, 1);
    }

    _check() {
      const entries = [];
      for (const target of this._targets) {
        const [, , width, height] = api.rect(target._id) || [0, 0, 0, 0];
        const previous = this._seen.get(target._id);
        // The first observation always fires, which is what a browser does and
        // what layout code depends on for its initial measurement.
        if (previous && previous.width === width && previous.height === height) continue;
        this._seen.set(target._id, { width, height });
        entries.push({
          target,
          contentRect: { x: 0, y: 0, width, height, top: 0, left: 0, right: width, bottom: height },
          borderBoxSize: [{ inlineSize: width, blockSize: height }],
          contentBoxSize: [{ inlineSize: width, blockSize: height }],
        });
      }
      if (entries.length) deliverTo(this, entries);
    }
  }

  function deliverTo(observer, entries) {
    try {
      observer._callback(entries, observer);
    } catch (error) {
      console.error("observer callback threw: " + withStack(error));
    }
  }

  // Called by the host after layout, once per settle round.
  globalThis.__h5iRunLayoutObservers = function () {
    if (intersectionObservers.length === 0 && resizeObservers.length === 0) return 0;
    const view = api.viewport();
    let ran = 0;
    for (const observer of intersectionObservers.slice()) { observer._check(view); ran++; }
    for (const observer of resizeObservers.slice()) { observer._check(); ran++; }
    return ran;
  };

  // ── mutation observation ─────────────────────────────────────────────────
  //
  // Records are produced by the mutating methods above rather than by polling
  // the tree, because those methods are the only way script can change it. That
  // is also the honest limit: a change made by the *parser* (an external script
  // arriving, say) is not observed, so this reports what script did rather than
  // everything that happened. Callbacks are delivered as a microtask, which is
  // when a real browser delivers them and what lets a framework batch.

  const observers = [];
  let deliveryQueued = false;

  class MutationObserver {
    constructor(callback) { this._callback = callback; this._records = []; this._targets = []; }
    observe(target, options) {
      this._targets.push({ target, options: options || { childList: true } });
      if (!observers.includes(this)) observers.push(this);
    }
    disconnect() {
      this._targets.length = 0;
      const at = observers.indexOf(this);
      if (at >= 0) observers.splice(at, 1);
    }
    takeRecords() { const r = this._records; this._records = []; return r; }
  }

  function observes(entry, record) {
    const { target, options } = entry;
    // Observing the *document* is what a framework does to watch a whole page —
    // Vite's module-preload polyfill opens with exactly this — and `document`
    // is not a `Node` here, so it has no `contains`. Calling it threw "not a
    // callable function" from inside this engine, on every page that mutated
    // the DOM after registering such an observer.
    const inScope = target.nodeType === 9
      ? record.target.isConnected
      : options.subtree
        ? target.contains(record.target)
        : target._id === record.target._id;
    if (!inScope) return false;
    if (record.type === "childList") return !!options.childList;
    if (record.type === "attributes") {
      if (!options.attributes) return false;
      const filter = options.attributeFilter;
      return !filter || filter.includes(record.attributeName);
    }
    if (record.type === "characterData") return !!options.characterData;
    return false;
  }

  function record(mutation) {
    if (observers.length === 0) return;
    let queued = false;
    for (const observer of observers) {
      if (observer._targets.some((entry) => observes(entry, mutation))) {
        observer._records.push(mutation);
        queued = true;
      }
    }
    if (queued && !deliveryQueued) {
      deliveryQueued = true;
      Promise.resolve().then(deliver);
    }
  }

  function deliver() {
    deliveryQueued = false;
    for (const observer of observers.slice()) {
      const records = observer.takeRecords();
      if (records.length === 0) continue;
      try {
        observer._callback(records, observer);
      } catch (error) {
        console.error("MutationObserver callback threw: " + withStack(error));
      }
    }
  }

  function recordAttribute(target, attributeName, oldValue) {
    if (observers.length === 0) return;
    record({
      type: "attributes", target, addedNodes: [], removedNodes: [],
      attributeName, oldValue,
    });
  }

  function childListRecord(target, added, removed) {
    // Built only if something will read it: the arrays and the object are pure
    // waste on a page with no observer, and every insertion made one.
    if (observers.length === 0) return;
    record({
      type: "childList",
      target,
      addedNodes: added || [],
      removedNodes: removed || [],
      attributeName: null,
      oldValue: null,
    });
  }

  // ── document and window ──────────────────────────────────────────────────

  // ── traversal ────────────────────────────────────────────────────────────
  //
  // `whatToShow` is a bitmask over node types, where the bit is `1 << (type-1)`:
  // element 1, text 4, comment 128. That arithmetic is the whole filter, plus
  // the caller's own function.

  const NodeFilter = {
    SHOW_ALL: 0xffffffff,
    SHOW_ELEMENT: 1,
    SHOW_TEXT: 4,
    SHOW_COMMENT: 128,
    FILTER_ACCEPT: 1,
    FILTER_REJECT: 2,
    FILTER_SKIP: 3,
  };

  function accepts(node, whatToShow, filter) {
    if (!node) return false;
    if (!(whatToShow & (1 << (node.nodeType - 1)))) return false;
    if (!filter) return true;
    const verdict = typeof filter === "function" ? filter(node) : filter.acceptNode(node);
    return verdict === NodeFilter.FILTER_ACCEPT;
  }

  /// Shared by both traversal objects: the subtree rooted at `root`, filtered.
  function traversable(root, whatToShow, filter) {
    const out = [];
    const visit = (id) => {
      const node = wrap(id);
      if (accepts(node, whatToShow, filter)) out.push(node);
      for (const kid of api.children(id)) visit(kid);
    };
    visit(root._id);
    return out;
  }

  class NodeIterator {
    constructor(root, whatToShow, filter) {
      this.root = root;
      this.whatToShow = whatToShow;
      this.filter = filter;
      this.referenceNode = root;
      this._at = -1;
    }
    _list() { return traversable(this.root, this.whatToShow, this.filter); }
    nextNode() {
      const list = this._list();
      this._at += 1;
      if (this._at >= list.length) { this._at = list.length; return null; }
      this.referenceNode = list[this._at];
      return this.referenceNode;
    }
    previousNode() {
      const list = this._list();
      this._at -= 1;
      if (this._at < 0) { this._at = -1; return null; }
      this.referenceNode = list[this._at];
      return this.referenceNode;
    }
    detach() {}
  }

  class TreeWalker {
    constructor(root, whatToShow, filter) {
      this.root = root;
      this.whatToShow = whatToShow;
      this.filter = filter;
      this.currentNode = root;
    }
    _list() { return traversable(this.root, this.whatToShow, this.filter); }
    _step(by) {
      const list = this._list();
      const at = list.findIndex((n) => n._id === this.currentNode._id);
      const next = list[(at < 0 ? (by > 0 ? -1 : list.length) : at) + by];
      if (!next) return null;
      this.currentNode = next;
      return next;
    }
    nextNode() { return this._step(1); }
    previousNode() { return this._step(-1); }
    parentNode() {
      for (let n = this.currentNode.parentNode; n; n = n.parentNode) {
        if (accepts(n, this.whatToShow, this.filter)) { this.currentNode = n; return n; }
        if (n._id === this.root._id) break;
      }
      return null;
    }
    firstChild() {
      for (const kid of this.currentNode.childNodes) {
        if (accepts(kid, this.whatToShow, this.filter)) { this.currentNode = kid; return kid; }
      }
      return null;
    }
    nextSibling() {
      for (let n = this.currentNode.nextSibling; n; n = n.nextSibling) {
        if (accepts(n, this.whatToShow, this.filter)) { this.currentNode = n; return n; }
      }
      return null;
    }
    previousSibling() {
      for (let n = this.currentNode.previousSibling; n; n = n.previousSibling) {
        if (accepts(n, this.whatToShow, this.filter)) { this.currentNode = n; return n; }
      }
      return null;
    }
  }

  /// The parts of `Range` pages actually use.
  ///
  /// Two of them, really: `createContextualFragment`, which is how a library
  /// turns a string of markup into nodes, and `getBoundingClientRect`, which is
  /// how it measures text. The rest of the interface is a selection model this
  /// engine has no use for, and anything reached for beyond what is here
  /// reports itself rather than silently doing nothing.
  /// Where a node sits among its siblings.
  function nodeIndex(node) {
    const parent = node && node.parentNode;
    if (!parent) return 0;
    const kids = parent.childNodes;
    for (let i = 0; i < kids.length; i++) if (kids[i]._id === node._id) return i;
    return 0;
  }

  function sameNode(a, b) {
    return !!a && !!b && a._id !== undefined && a._id === b._id;
  }

  /// Every node at or under `node`, in document order.
  function flattenTree(node, out = []) {
    if (!node) return out;
    out.push(node);
    if (node.nodeType === 1 || node.nodeType === 9 || node.nodeType === 11) {
      for (const kid of node.childNodes) flattenTree(kid, out);
    }
    return out;
  }

  /// A range over the document, with the offsets a real one has.
  ///
  /// The version this replaces stored two containers, ignored every offset it
  /// was given, and answered `toString()` with the start container's entire
  /// `textContent`. That is fine until something depends on it — and Selection
  /// and `execCommand` depend on nothing else, because a selection *is* a pair
  /// of boundary points.
  ///
  /// Boundary points are compared by flattening the common ancestor in document
  /// order rather than by a general position comparison. That is enough for what
  /// runs through here and it is honest about its shape: ranges whose ends sit
  /// in unrelated trees are not ordered by this, and nothing asks them to be.
  class Range {
    constructor() {
      this.startContainer = document;
      this.startOffset = 0;
      this.endContainer = document;
      this.endOffset = 0;
    }
    get collapsed() {
      return sameNode(this.startContainer, this.endContainer)
        ? this.startOffset === this.endOffset
        : this.startContainer === this.endContainer && this.startOffset === this.endOffset;
    }
    setStart(node, offset) { this.startContainer = node; this.startOffset = Number(offset) || 0; }
    setEnd(node, offset) { this.endContainer = node; this.endOffset = Number(offset) || 0; }
    setStartBefore(node) { this.setStart(node.parentNode, nodeIndex(node)); }
    setStartAfter(node) { this.setStart(node.parentNode, nodeIndex(node) + 1); }
    setEndBefore(node) { this.setEnd(node.parentNode, nodeIndex(node)); }
    setEndAfter(node) { this.setEnd(node.parentNode, nodeIndex(node) + 1); }
    selectNode(node) { this.setStartBefore(node); this.setEndAfter(node); }
    selectNodeContents(node) {
      this.setStart(node, 0);
      this.setEnd(node, node.nodeType === 3 ? node.data.length : node.childNodes.length);
    }
    collapse(toStart) {
      if (toStart) { this.endContainer = this.startContainer; this.endOffset = this.startOffset; }
      else { this.startContainer = this.endContainer; this.startOffset = this.endOffset; }
    }
    cloneRange() {
      const copy = new Range();
      copy.startContainer = this.startContainer; copy.startOffset = this.startOffset;
      copy.endContainer = this.endContainer; copy.endOffset = this.endOffset;
      return copy;
    }
    detach() {}

    get commonAncestorContainer() {
      const ancestors = [];
      for (let n = this.startContainer; n; n = n.parentNode) ancestors.push(n);
      for (let n = this.endContainer; n; n = n.parentNode) {
        if (ancestors.some((a) => sameNode(a, n) || a === n)) return n;
      }
      return document;
    }

    /// The nodes this range covers, with any partial text pieces resolved.
    ///
    /// Returns entries of `{ node, text, whole }`: `whole` marks a node that is
    /// entirely inside the range and can simply be removed, which is what makes
    /// `deleteContents` and `toString` share one traversal instead of
    /// disagreeing about what "inside" means.
    _pieces() {
      const flat = flattenTree(this.commonAncestorContainer);
      const at = (node) => flat.findIndex((n) => sameNode(n, node) || n === node);

      // A boundary point is either *inside* a text node, or *between* two
      // children of an element. Resolving both to a position in the flattened
      // list is the whole trick: without it, `selectNodeContents(p)` — whose
      // boundaries are both the element `p` — covered no text node at all and
      // the selection read as empty.
      const resolve = (container, offset) => {
        if (container.nodeType === 3) return { index: at(container), textOffset: offset };
        const here = at(container);
        if (here < 0) return { index: -1, textOffset: null };
        const kids = container.childNodes;
        if (offset < kids.length) return { index: at(kids[offset]), textOffset: null };
        // Past the last child: the position just after this whole subtree.
        return { index: here + flattenTree(container).length, textOffset: null };
      };

      const from = resolve(this.startContainer, this.startOffset);
      const to = resolve(this.endContainer, this.endOffset);
      if (from.index < 0 || to.index < 0) return [];

      // An element end boundary sits *before* the node at its index; a text one
      // sits inside it.
      const last = to.textOffset === null ? to.index - 1 : to.index;
      const out = [];
      for (let i = from.index; i <= last; i++) {
        const node = flat[i];
        if (!node || node.nodeType !== 3) continue;
        const begin = i === from.index && from.textOffset !== null ? from.textOffset : 0;
        const finish = i === to.index && to.textOffset !== null ? to.textOffset : node.data.length;
        if (finish <= begin) continue;
        out.push({
          node,
          text: node.data.slice(begin, finish),
          begin,
          finish,
          whole: begin === 0 && finish >= node.data.length,
        });
      }
      return out;
    }

    toString() { return this._pieces().map((piece) => piece.text).join(""); }

    deleteContents() {
      // Reversed, so removing a node cannot shift the offsets of the pieces not
      // yet handled. Sliced by offset rather than by matching the text, because
      // `split(text).join("")` deleted every *other* occurrence of the same
      // string in the same node too.
      for (const piece of this._pieces().reverse()) {
        if (piece.whole) piece.node.remove();
        else piece.node.data = piece.node.data.slice(0, piece.begin) + piece.node.data.slice(piece.finish);
      }
      this.collapse(true);
    }

    /// Put a node at the start boundary.
    insertNode(node) {
      const container = this.startContainer;
      if (container.nodeType === 3) {
        // Split the text so the node lands exactly where the boundary is,
        // rather than before or after the whole run.
        const after = container.splitText
          ? container.splitText(this.startOffset)
          : null;
        const parent = container.parentNode;
        if (!parent) return node;
        if (after) parent.insertBefore(node, after);
        else parent.appendChild(node);
        return node;
      }
      const kids = container.childNodes;
      if (this.startOffset < kids.length) container.insertBefore(node, kids[this.startOffset]);
      else container.appendChild(node);
      return node;
    }

    surroundContents(wrapper) {
      const text = this.toString();
      this.deleteContents();
      wrapper.textContent = text;
      this.insertNode(wrapper);
      return wrapper;
    }

    extractContents() {
      const fragment = new DocumentFragment();
      const text = this.toString();
      this.deleteContents();
      if (text) fragment.appendChild(document.createTextNode(text));
      return fragment;
    }
    cloneContents() {
      const fragment = new DocumentFragment();
      const text = this.toString();
      if (text) fragment.appendChild(document.createTextNode(text));
      return fragment;
    }

    getBoundingClientRect() {
      const anchor = this.startContainer && this.startContainer.nodeType === 3
        ? this.startContainer.parentNode
        : this.startContainer;
      return anchor && anchor.getBoundingClientRect
        ? anchor.getBoundingClientRect()
        : { x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0 };
    }
    getClientRects() { return [this.getBoundingClientRect()]; }
    createContextualFragment(html) {
      const host = document.createElement("div");
      host.innerHTML = String(html);
      const fragment = new DocumentFragment();
      for (const kid of host.childNodes) fragment.appendChild(kid);
      return fragment;
    }
  }

  /// The document's selection: one range, which is all a browser gives you.
  ///
  /// Agents need this to act on text — "select this paragraph, replace it" is
  /// the shape of a great deal of real work — and `execCommand` below is
  /// defined entirely in terms of it.
  ///
  /// Multiple ranges are not supported and say so by reporting `rangeCount`
  /// honestly: only Gecko ever implemented them, and code that asks for range
  /// two is code written against a browser this is not.
  class Selection {
    constructor() { this._range = null; this._direction = "forward"; }

    get rangeCount() { return this._range ? 1 : 0; }
    get isCollapsed() { return !this._range || this._range.collapsed; }
    get type() {
      if (!this._range) return "None";
      return this._range.collapsed ? "Caret" : "Range";
    }
    get anchorNode() { return this._range ? this._range.startContainer : null; }
    get anchorOffset() { return this._range ? this._range.startOffset : 0; }
    get focusNode() { return this._range ? this._range.endContainer : null; }
    get focusOffset() { return this._range ? this._range.endOffset : 0; }

    getRangeAt(index) {
      if (Number(index) !== 0 || !this._range) {
        throw new DOMException(`there is no range ${index}`, "IndexSizeError");
      }
      return this._range;
    }
    addRange(range) { if (!this._range) this._range = range; }
    removeRange(range) { if (this._range === range) this._range = null; }
    removeAllRanges() { this._range = null; }
    empty() { this.removeAllRanges(); }

    collapse(node, offset) {
      if (node === null || node === undefined) return this.removeAllRanges();
      const range = new Range();
      range.setStart(node, offset || 0);
      range.collapse(true);
      this._range = range;
    }
    setPosition(node, offset) { this.collapse(node, offset); }
    collapseToStart() {
      if (!this._range) throw new DOMException("nothing is selected", "InvalidStateError");
      this._range.collapse(true);
    }
    collapseToEnd() {
      if (!this._range) throw new DOMException("nothing is selected", "InvalidStateError");
      this._range.collapse(false);
    }
    extend(node, offset) {
      if (!this._range) throw new DOMException("nothing is selected", "InvalidStateError");
      this._range.setEnd(node, offset || 0);
    }
    setBaseAndExtent(anchor, anchorOffset, focus, focusOffset) {
      const range = new Range();
      range.setStart(anchor, anchorOffset);
      range.setEnd(focus, focusOffset);
      this._range = range;
    }
    selectAllChildren(node) {
      const range = new Range();
      range.selectNodeContents(node);
      this._range = range;
    }
    deleteFromDocument() { if (this._range) this._range.deleteContents(); }
    containsNode(node, partly) {
      if (!this._range || !node) return false;
      const covered = flattenTree(this._range.commonAncestorContainer);
      const inside = covered.some((n) => sameNode(n, node));
      if (!inside) return false;
      if (partly) return true;
      const text = this._range.toString();
      return text.includes(node.textContent || "");
    }
    toString() { return this._range ? this._range.toString() : ""; }
  }

  // ── what this file hands out is watched, once ────────────────────────────
  //
  // Four classes rather than four objects per read. The sentinel goes at the
  // end of each class's own prototype chain, so a method or accessor the class
  // really has is found before anything is trapped, and only a read that missed
  // arrives at the report. See `observedClass`.
  //
  // `Node` covers every node, because `Element`, `Text` and `Comment` all
  // descend from it — and its label comes from the receiver, since one chain
  // ends three kinds of node and "reading `tagName` off a text node" has to
  // stay distinguishable from reading it off an element.
  internals.withStack = withStack;
  observedClass(Node, (receiver) => KIND_LABELS[receiver._kind] ?? null);
  declareInternals(Node.prototype, ["_kind"]);
  declareInternals(Element.prototype, [
    // Set by `createElementNS`, and by nothing the parser does.
    "_nsuri", "_prefix", "_localName",
    // The tag, remembered on first read: see `get tagName`.
    "_tag",
    // The token list a `classList` read memoises on its element.
    "__h5iClassList",
    // A form control's dirty overlay: present only once something set it.
    "_checked", "_value", "_selected",
    // The shadow root an element may have been given, and usually was not.
    "_shadow",
  ]);
  observedClass(DOMTokenList, "DOMTokenList");
  observedClass(Range, "Range");
  observedClass(Selection, "Selection");

  const selection = new Selection();
  function getSelection() { return selection; }

  /// `document.execCommand`, for the commands this engine can actually carry out.
  ///
  /// Deprecated, never converged across browsers, and still the only way a page
  /// edits a `contenteditable` region — which is what an agent driving a rich
  /// text editor has to go through. So: a small set, done properly, and
  /// everything else answers **false** from both `execCommand` and
  /// `queryCommandSupported` rather than returning true and doing nothing. A
  /// command that reports success without acting is the failure this engine
  /// keeps removing.
  const COMMANDS = {
    bold: (sel) => wrapSelection(sel, "b"),
    italic: (sel) => wrapSelection(sel, "i"),
    underline: (sel) => wrapSelection(sel, "u"),
    strikethrough: (sel) => wrapSelection(sel, "s"),
    subscript: (sel) => wrapSelection(sel, "sub"),
    superscript: (sel) => wrapSelection(sel, "sup"),

    inserttext: (sel, value) => replaceSelection(sel, document.createTextNode(String(value ?? ""))),
    inserthtml: (sel, value) => {
      const host = document.createElement("div");
      host.innerHTML = String(value ?? "");
      const fragment = new DocumentFragment();
      for (const kid of [...host.childNodes]) fragment.appendChild(kid);
      return replaceSelection(sel, fragment);
    },
    insertlinebreak: (sel) => replaceSelection(sel, document.createElement("br")),
    insertparagraph: (sel) => replaceSelection(sel, document.createElement("p")),

    delete: (sel) => { if (!sel.rangeCount) return false; sel.getRangeAt(0).deleteContents(); return true; },
    forwarddelete: (sel) => COMMANDS.delete(sel),

    selectall: (sel) => {
      const body = wrap(api.body());
      if (!body) return false;
      sel.selectAllChildren(body);
      return true;
    },

    createlink: (sel, value) => {
      if (!sel.rangeCount || sel.isCollapsed) return false;
      const link = document.createElement("a");
      link.setAttribute("href", String(value ?? ""));
      return wrapSelectionWith(sel, link);
    },
    unlink: (sel) => {
      if (!sel.rangeCount) return false;
      const range = sel.getRangeAt(0);
      let found = false;
      for (let n = range.startContainer; n; n = n.parentNode) {
        if (n.nodeType === 1 && n.tagName === "A") {
          const text = document.createTextNode(n.textContent);
          n.parentNode.insertBefore(text, n);
          n.remove();
          found = true;
          break;
        }
      }
      return found;
    },

    formatblock: (sel, value) => {
      const tag = String(value ?? "").replace(/[<>]/g, "").toLowerCase();
      if (!tag) return false;
      return wrapSelectionWith(sel, document.createElement(tag));
    },
  };

  function replaceSelection(sel, node) {
    if (!sel.rangeCount) return false;
    const range = sel.getRangeAt(0);
    range.deleteContents();
    range.insertNode(node);
    return true;
  }

  function wrapSelectionWith(sel, wrapper) {
    if (!sel.rangeCount) return false;
    const range = sel.getRangeAt(0);
    if (range.collapsed) return true;
    range.surroundContents(wrapper);
    return true;
  }

  function wrapSelection(sel, tag) {
    return wrapSelectionWith(sel, document.createElement(tag));
  }

  /// `document.implementation`, for whichever document asked.
  function domImplementation(owner) {
    return observed({
      hasFeature: () => true,
      /// A doctype is three strings and a nodeType; refusing it was never a capability
      /// question.
      createDocumentType: (name, publicId, systemId) => {
        const text = String(name);
        if (/[>\s]/.test(text)) {
          throw new DOMException(
            `\`${text}\` would not survive serialising as \`<!DOCTYPE ${text}>\``,
            "InvalidCharacterError",
          );
        }
        return new DocumentTypeNode(text, String(publicId), String(systemId), owner);
      },
      // The same shape `DOMParser` produces, which is what this is for: a
      // detached document to build markup in. It shares this engine's one tree,
      // so it is a subtree presented as a document rather than a second one —
      // enough for building and querying, which is all it is used for.
      createHTMLDocument: (title) =>
        new DOMParser().parseFromString(
          `<title>${String(title ?? "")}</title>`, "text/html",
        ),
      // A second *live* document is genuinely out of reach — there is one tree,
      // and it is the page.
    }, "document.implementation");
  }

  /// `new DOMParser().parseFromString(html, "text/html")`.
  ///
  /// How a library turns a string of markup into something it can query —
  /// sanitizers, template engines and lit all do it. What comes back is a
  /// parsed subtree presented as a document, not a second document: this engine
  /// has one tree, so the result shares its arena. That is enough for reading
  /// and querying, which is all this is used for, and no script inside the
  /// string runs — which is the property a sanitizer is relying on anyway.
  class DOMParser {
    parseFromString(markup, type) {
      const kind = String(type || "text/html").toLowerCase();
      if (kind !== "text/html" && kind !== "application/xhtml+xml") {
        api.unsupported(`DOMParser.parseFromString(${kind})`);
      }
      const root = document.createElement("html");
      const head = document.createElement("head");
      const body = document.createElement("body");
      root.appendChild(head);
      root.appendChild(body);
      body.innerHTML = String(markup);

      return observed({
        documentElement: root,
        body,
        // A real parsed document always has a head, even for a fragment of
        // markup that contains none. Returning null here was enough to take
        // preactjs.com's markup component down with a null dereference, and the
        // page then re-rendered everything it had already server-rendered.
        head,
        nodeType: 9,
        contentType: kind,
        get implementation() { return domImplementation(this); },
        get title() {
          const found = body.querySelector("title");
          return found ? found.textContent : "";
        },
        querySelector: (sel) => root.querySelector(sel),
        querySelectorAll: (sel) => root.querySelectorAll(sel),
        getElementById: (id) => root.querySelector("#" + String(id)),
        getElementsByTagName: (tag) => root.getElementsByTagName(tag),
        getElementsByClassName: (cls) => root.getElementsByClassName(cls),
        createDocumentFragment: () => new DocumentFragment(),
        createComment: (text) => document.createComment(text),
        createElement: (tag) => document.createElement(tag),
        createTextNode: (text) => document.createTextNode(text),
        importNode: (node, deep) => node.cloneNode(deep),
        adoptNode: (node) => node,
      }, "parsed document");
    }
  }

  let adoptedSheets = [];

  const documentImpl = {
    get documentElement() { return wrap(api.root()); },

    // `document.dir` is not the document's own attribute: it reflects
    // `<html dir>`, so reading it off the document and reading it off
    // `documentElement` have to agree. It was absent, and the reflection suite
    // tests it as `#document.dir (<html dir>)`.
    get dir() {
      const root = wrap(api.root());
      return root ? root.dir : "";
    },
    set dir(value) {
      const root = wrap(api.root());
      if (root) root.dir = value;
    },
    get body() { return wrap(api.body()); },
    get head() { return wrap(api.query("head", 0)); },
    /// `createElement("a", { is: "fancy-link" })` builds a customized built-in.
    /// The attribute is stamped *before* the wrapper is made, because the
    /// upgrade reads `is` to find its definition.
    createElement(tag, options) {
      const id = api.createElement(String(tag));
      if (options && options.is != null) api.setAttr(id, "is", String(options.is));
      return wrap(id);
    },
    // SVG and MathML arrive through this, and every framework that draws an
    // icon calls it. The namespace is dropped because this engine models one:
    // the element is created under its local name, which is what the renderer
    // can do something with.
    /// `createElementNS`, with the two validations that are most of what it is.
    createElementNS(namespace, qualifiedName) {
      const ns = namespace === null || namespace === undefined ? null : String(namespace);
      const name = String(qualifiedName);
      validateQualifiedName(name);
      const colon = name.indexOf(":");
      const prefix = colon === -1 ? null : name.slice(0, colon);
      if (prefix !== null && ns === null) {
        throw new DOMException(
          `\`${name}\` has a prefix, so it needs a namespace`,
          "NamespaceError",
        );
      }
      if (prefix === "xml" && ns !== "http://www.w3.org/XML/1998/namespace") {
        throw new DOMException(
          "the `xml` prefix belongs to the XML namespace",
          "NamespaceError",
        );
      }
      if ((name === "xmlns" || prefix === "xmlns")
        !== (ns === "http://www.w3.org/2000/xmlns/")) {
        throw new DOMException(
          "`xmlns` and the XMLNS namespace may only be used with each other",
          "NamespaceError",
        );
      }
      // The wrapper carries what the tree cannot: this engine's one tree is
      // an HTML tree, so the namespace, prefix and original-case local name
      // live on the JS wrapper (wrappers are cached by id, so the expandos
      // hold for the node's lifetime). That is enough for everything a page
      // reads back — `namespaceURI`, `prefix`, `localName`, a `tagName` that
      // is *not* uppercased outside the HTML namespace — while layout keeps
      // treating the element as the HTML-parsed name it stored.
      const local = colon === -1 ? name : name.slice(colon + 1);
      const wrapper = wrap(api.createElement(local));
      wrapper._nsuri = ns;
      wrapper._prefix = prefix;
      wrapper._localName = local;
      return wrapper;
    },
    // `document.write`, emulated where it can be and refused where it cannot.
    write(...parts) {
      const markup = parts.join("");
      const id = globalThis.__h5iCurrentScript;
      const script = id === null || id === undefined ? null : wrap(id);
      if (!script || !script.parentNode) {
        api.unsupported("document.write (after parsing)");
        return;
      }
      const host = document.createElement("div");
      host.innerHTML = markup;
      const parent = script.parentNode;
      const next = script.nextSibling;
      for (const kid of host.childNodes) {
        if (next) parent.insertBefore(kid, next);
        else parent.appendChild(kid);
      }
    },
    writeln(...parts) { this.write(...parts, "\n"); },
    // `open` and `close` exist so a page that brackets its writes does not throw
    // on the bracket. Neither replaces the document, for the reason above.
    open() { return document; },
    close() {},

    createRange() { return new Range(); },
    getSelection() { return getSelection(); },

    /// The commands this engine carries out, and no others.
    ///
    /// `queryCommandSupported` answers from the same table `execCommand` acts
    /// on, so the two can never disagree — a page that asks first and acts
    /// second gets one consistent story.
    execCommand(name, _showUI, value) {
      const key = String(name ?? "").toLowerCase();
      const command = COMMANDS[key];
      if (!command) {
        api.unsupported(`document.execCommand(${key})`);
        return false;
      }
      try {
        return !!command(selection, value);
      } catch (error) {
        console.error(`execCommand(${key}) threw: ${withStack(error)}`);
        return false;
      }
    },
    queryCommandSupported(name) {
      return Object.prototype.hasOwnProperty.call(COMMANDS, String(name ?? "").toLowerCase());
    },
    queryCommandEnabled(name) {
      return this.queryCommandSupported(name) && selection.rangeCount > 0;
    },
    /// Always false, and honest about why: this engine keeps no record of the
    /// formatting around the caret, so "is the selection bold" is a question it
    /// cannot answer. Returning a guess would be worse than returning false —
    /// an editor toolbar would light up at random.
    queryCommandState(name) {
      api.unsupported(`document.queryCommandState(${String(name ?? "")})`);
      return false;
    },
    queryCommandValue(name) {
      api.unsupported(`document.queryCommandValue(${String(name ?? "")})`);
      return "";
    },
    // The pre-constructor way of making an event, still emitted by older
    // libraries and by anything compiled for old targets. The event is inert
    // until `initEvent` names it, which is exactly how the legacy API works.
    /// `createEvent`, with the table the spec carries rather than a generic Event for every
    /// name.
    createEvent(kind) {
      const table = {
        "beforeunloadevent": BeforeUnloadEvent,
        "compositionevent": CompositionEvent,
        "customevent": CustomEvent,
        "devicemotionevent": DeviceMotionEvent,
        "deviceorientationevent": DeviceOrientationEvent,
        "dragevent": DragEvent,
        "event": Event,
        "events": Event,
        "focusevent": FocusEvent,
        "hashchangeevent": HashChangeEvent,
        "htmlevents": Event,
        "keyboardevent": KeyboardEvent,
        "messageevent": MessageEvent,
        "mouseevent": MouseEvent,
        "mouseevents": MouseEvent,
        "storageevent": StorageEvent,
        "svgevents": Event,
        "textevent": TextEvent,
        "uievent": UIEvent,
        "uievents": UIEvent,
      };
      const key = String(kind).replace(/[A-Z]/g, (c) => c.toLowerCase());
      const Ctor = table[key];
      if (!Ctor) {
        throw new DOMException(
          `createEvent(${String(kind)}) is not on the legacy table; construct the ` +
            "event with `new` instead",
          "NotSupportedError",
        );
      }
      const event = new Ctor("", {});
      event.type = "";
      return event;
    },
    elementFromPoint(x, y) { return wrap(api.elementFromPoint(Number(x), Number(y))); },
    elementsFromPoint(x, y) {
      const found = wrap(api.elementFromPoint(Number(x), Number(y)));
      // The ancestors of the hit, topmost first, which is what the plural form
      // returns and what a library walking for a scroll container wants.
      const out = [];
      for (let n = found; n; n = n.parentNode) if (n.nodeType === 1) out.push(n);
      return collection(out);
    },
    createTextNode(text) { return wrap(api.createText(String(text))); },
    /// A detached attribute node. **`setAttributeNode` does not exist here**,
    /// so what comes back is inspectable and not yet installable; saying so is
    /// better than a comment promising a method the prelude has never had.
    createAttribute(name) {
      const lowered = String(name).toLowerCase();
      validateQualifiedName(lowered);
      return internal(() => new Attr(lowered, "", null));
    },
    createAttributeNS(namespace, qualifiedName) {
      const ns = namespace === null || namespace === undefined ? null : String(namespace);
      const qname = String(qualifiedName);
      validateQualifiedName(qname);
      const at = qname.indexOf(":");
      return internal(() => new Attr(qname, "", null, ns,
        at === -1 ? null : qname.slice(0, at)));
    },
    createDocumentFragment() { return new DocumentFragment(); },
    /// Validated twice, because the two rules guard different attacks: the
    /// target must be a Name (`InvalidCharacterError`), and the data must not
    /// contain `?>` — which would end the instruction early on serialisation
    /// and turn the rest of the data into markup.
    createProcessingInstruction(target, data) {
      const name = String(target);
      validateQualifiedName(name);
      const text = String(data);
      if (text.includes("?>")) {
        throw new DOMException(
          "the data of a processing instruction must not contain \"?>\"",
          "InvalidCharacterError",
        );
      }
      return new ProcessingInstructionNode(name, text);
    },
    createComment(text) {
      const id = api.createComment(String(text));
      comments.add(id);
      return wrap(id);
    },
    // One document means importing is cloning. Saying that plainly is better
    // than a stub that returns nothing and leaves the caller inserting null.
    importNode(node, deep) { return node.cloneNode(deep); },
    adoptNode(node) { return node; },
    createNodeIterator(root, whatToShow, filter) {
      return new NodeIterator(root, whatToShow === undefined ? NodeFilter.SHOW_ALL : whatToShow, filter);
    },
    createTreeWalker(root, whatToShow, filter) {
      return new TreeWalker(root, whatToShow === undefined ? NodeFilter.SHOW_ALL : whatToShow, filter);
    },
    // Real API in its own right: `document.contains(node)` is how code asks
    // whether something is still on the page.
    contains(node) {
      return !!node && node.isConnected === true;
    },
    getElementsByName(name) {
      return api.queryAll(`[name="${String(name).replace(/"/g, '\\"')}"]`, 0).map(wrap);
    },
    querySelector(sel) { return withHasMarkers(sel, (t) => wrap(api.query(t, 0))); },
    querySelectorAll(sel) { return withHasMarkers(sel, (t) => collection(api.queryAll(t, 0).map(wrap))); },
    getElementById(id) { return wrap(api.query("#" + String(id), 0)); },
    getElementsByTagName(tag) { return collection(api.queryAll(String(tag), 0).map(wrap), "HTMLCollection"); },
    getElementsByClassName(cls) { return collection(api.queryAll("." + String(cls), 0).map(wrap), "HTMLCollection"); },
    addEventListener(type, handler, options) {
      const root = wrap(api.root());
      if (root) root.addEventListener(type, handler, options);
    },
    removeEventListener(type, handler) {
      const root = wrap(api.root());
      if (root) root.removeEventListener(type, handler);
    },
    // Non-HttpOnly cookies only, exactly as a browser exposes them. The
    // withholding is the point: a session credential is almost always HttpOnly,
    // and anything script can read it can write into the DOM, where the agent
    // reads it.
    get cookie() { return api.readCookies(); },
    set cookie(value) { api.writeCookie(String(value)); },
    get readyState() { return documentReadyState; },

    // A document is node type 9 and its child is the root element. Scripts that
    // walk upward from a node and stop at the document depend on both.
    get nodeType() { return 9; },
    get nodeName() { return "#document"; },
    get childNodes() { const root = wrap(api.root()); return root ? [root] : []; },
    get defaultView() { return globalThis; },
    get location() { return location; },
    get URL() { return currentAddress; },
    get documentURI() { return currentAddress; },
    // What relative URLs on this page resolve against — the `<base href>` if
    // the page set one, and the address otherwise.
    get baseURI() {
      const base = wrap(api.query("base[href]", 0));
      if (!base) return currentAddress;
      const parts = api.parseUrl(api.getAttr(base._id, "href") || "", currentAddress);
      return parts ? parts.href : currentAddress;
    },
    // This engine parses HTML and nothing else, so there is one honest answer.
    contentType: "text/html",
    /// What this document was decoded as. All three names are the same value
    /// and all three are in use: `characterSet` is current, `charset` is the
    /// legacy alias, and `inputEncoding` is the one the DOM spec kept.
    get characterSet() { return api.documentEncoding(); },
    get charset() { return api.documentEncoding(); },
    get inputEncoding() { return api.documentEncoding(); },
    // Adopting a sheet applies it. Assignment replaces the set, as in a browser.
    /// What scrolls when the document scrolls. In standards mode that is
    /// `<html>`, and code reads it to avoid the quirks-mode `<body>` split.
    get scrollingElement() { return wrap(api.root()); },
    /// Every `<style>` and `<link rel=stylesheet>` the document has, as sheets.
    get styleSheets() { return styleSheetList(); },
    get adoptedStyleSheets() { return adoptedSheets.slice(); },
    set adoptedStyleSheets(sheets) {
      adoptedSheets = Array.from(sheets || []);
      for (const sheet of adoptedSheets) if (sheet && sheet._apply) sheet._apply();
    },

    // Empty, and true: this engine sends no `Referer`, so a page told anything
    // else would be told a lie about a request it can check.
    get referrer() { return ""; },

    get title() {
      const el = wrap(api.query("title", 0));
      return el ? el.textContent : "";
    },
    set title(value) {
      let el = wrap(api.query("title", 0));
      if (!el) {
        const head = wrap(api.query("head", 0));
        if (!head) return;
        el = document.createElement("title");
        head.appendChild(el);
      }
      el.textContent = String(value);
    },

    // Set by the host around each classic script, null inside a module or a
    // later callback — the same rule a browser follows.
    get currentScript() {
      const id = globalThis.__h5iCurrentScript;
      return id === null || id === undefined ? null : wrap(id);
    },

    get forms() { return api.queryAll("form", 0).map(wrap); },
    get images() { return api.queryAll("img", 0).map(wrap); },
    get scripts() { return api.queryAll("script", 0).map(wrap); },
    // Only anchors that actually have an href, which is what the collection is
    // defined to hold — a named anchor is not a link.
    get links() { return api.queryAll("a[href], area[href]", 0).map(wrap); },

    // Nothing is focused until something is: this engine has no focus ring, and
    // the body is what a browser reports in that state.
    // A Document has no `namespaceURI` and its `ownerDocument` is null — both
    // are true of a real browser. Defined rather than absent so the reporting
    // proxy does not name them as gaps: something no engine has is not
    // something this engine is missing.
    namespaceURI: undefined,
    ownerDocument: null,
    get implementation() { return domImplementation(null); },

    /// **"CSS1Compat", and that is a fact about this engine rather than a
    /// guess.** Quirks mode is a parse-time decision, and this engine parses in
    /// no-quirks mode unconditionally — `QuirksMode::NoQuirks` is hard-coded at
    /// every place style is parsed. So the standards-mode answer is what it
    /// actually does, not what it hopes the page had.
    get compatMode() { return "CSS1Compat"; },

    // The famous one. Legacy code uses `document.all` to detect old IE, and the
    // detection works because it is the only object in JavaScript that is
    // falsy while being an object. That cannot be reproduced here — Boa has no
    // `[[IsHTMLDDA]]` — so this returns the collection and *not* the falsiness,
    // which is the honest half: a page feature-detecting with it will take the
    // "modern browser" branch, which is the correct one for this engine.
    get all() { return collection(api.queryAll("*", 0).map(wrap), "HTMLCollection"); },

    /// What has focus. The body when nothing does, as in a browser — never
    /// null, which is what code branching on it expects.
    get activeElement() {
      if (focusedId !== null) {
        const focused = wrap(focusedId);
        if (focused && focused.isConnected) return focused;
        focusedId = null;
      }
      return wrap(api.body());
    },
    get hidden() { return false; },
    get visibilityState() { return "visible"; },
  };

  // The legacy colour properties: aliases for attributes *on the body*, kept
  // because `reflection-sections.html` tests every one on `#document` and old
  // pages still write them. All five carry [LegacyNullToEmptyString], and all
  // five read "" with no body rather than throwing on one.
  for (const [idl, attr] of [
    ["fgColor", "text"], ["bgColor", "bgcolor"], ["linkColor", "link"],
    ["alinkColor", "alink"], ["vlinkColor", "vlink"],
  ]) {
    Object.defineProperty(documentImpl, idl, {
      configurable: true,
      enumerable: true,
      get() {
        const body = wrap(api.body());
        if (!body) return "";
        return api.getAttr(body._id, attr) ?? "";
      },
      set(value) {
        const body = wrap(api.body());
        if (!body) return;
        api.setAttr(body._id, attr, value === null ? "" : String(value));
      },
    });
  }

  // Same rule for `document`: a page reading `document.activeElement` or
  // `document.fonts` should produce a named gap, not a silent undefined.
  const document = observed(documentImpl, "document");
  // The document is passed wherever a node is, and every one of those paths
  // reads `._id` off it. It does not have one — there is no id that means "the
  // document" to the primitives — so each of those reads walked its whole chain
  // into the sentinel. 105 of them in one ordinary page's worth of work.
  declareInternals(documentImpl, ["_id"]);

  const console = {
    log: (...a) => api.log("log", a.map(render).join(" ")),
    info: (...a) => api.log("info", a.map(render).join(" ")),
    warn: (...a) => api.log("warn", a.map(render).join(" ")),
    error: (...a) => api.log("error", a.map(render).join(" ")),
    debug: (...a) => api.log("debug", a.map(render).join(" ")),
  };

  // ── base64 and the legacy escapes ────────────────────────────────────────
  //
  // Named by the corpus once ReferenceErrors could name themselves. Small
  // enough that a stub reporting them as missing would cost more than the
  // implementation, and a page encoding a data: URI or a basic-auth header
  // fails outright without them.
  const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

  function btoa(input) {
    const text = String(input);
    let out = "";
    for (let i = 0; i < text.length; i += 3) {
      const a = text.charCodeAt(i);
      const b = text.charCodeAt(i + 1);
      const c = text.charCodeAt(i + 2);
      // Byte-oriented by definition: btoa on a code point above 255 throws in
      // a browser rather than mangling it, and a page that catches that is
      // entitled to the same answer here.
      if (a > 255 || (b === b && b > 255) || (c === c && c > 255)) {
        throw new TypeError("btoa: the string contains characters outside of Latin1");
      }
      const triple = (a << 16) | ((b || 0) << 8) | (c || 0);
      out += B64[(triple >> 18) & 63] + B64[(triple >> 12) & 63]
        + (Number.isNaN(b) ? "=" : B64[(triple >> 6) & 63])
        + (Number.isNaN(c) ? "=" : B64[triple & 63]);
    }
    return out;
  }

  function atob(input) {
    const text = String(input).replace(/[ \t\n\f\r]/g, "").replace(/=+$/, "");
    let out = "";
    let bits = 0;
    let held = 0;
    for (const ch of text) {
      const value = B64.indexOf(ch);
      if (value < 0) throw new TypeError("atob: the string is not valid base64");
      held = (held << 6) | value;
      bits += 6;
      if (bits >= 8) {
        bits -= 8;
        out += String.fromCharCode((held >> bits) & 255);
      }
    }
    return out;
  }

  function escape(input) {
    return String(input).replace(/[^A-Za-z0-9@*_+\-./]/g, (ch) => {
      const code = ch.charCodeAt(0);
      return code < 256
        ? "%" + code.toString(16).toUpperCase().padStart(2, "0")
        : "%u" + code.toString(16).toUpperCase().padStart(4, "0");
    });
  }

  function unescape(input) {
    return String(input).replace(/%u([0-9a-fA-F]{4})|%([0-9a-fA-F]{2})/g, (_m, wide, byte) =>
      String.fromCharCode(parseInt(wide || byte, 16)));
  }

  // Defined rather than assigned, and the distinction is not cosmetic:
  // `Object.assign` invokes a getter and copies the value it returns, so a
  // scroll offset written that way freezes at whatever it was when the prelude
  // ran. Anything on the global object that changes over the life of the page
  // belongs here.
  //
  // These are also a gap the reporting proxy could never have found — nothing
  // wraps the global object, so `window.innerWidth` was simply undefined, and a
  // layout that measures instead of asking `matchMedia` got NaN out of its own
  // arithmetic.
  function defineLive(properties) {
    for (const [name, get] of Object.entries(properties)) {
      Object.defineProperty(globalThis, name, { get, configurable: true, enumerable: true });
    }
  }

  function render(v) {
    if (typeof v === "string") return v;
    if (v === null || v === undefined) return String(v);

    // An Error has no enumerable own properties, so `JSON.stringify` renders it
    // as `{}`. A page logging its own failures then fills the console with
    // hundreds of lines that say nothing — remix.run produced 1487 of them —
    // and the one thing an agent needed, the message, was the part thrown away.
    if (v instanceof Error || (typeof v?.message === "string" && typeof v?.name === "string")) {
      return v.stack ? `${v.name}: ${v.message}\n${v.stack}` : `${v.name}: ${v.message}`;
    }
    if (typeof v === "function") return `[function ${v.name || "anonymous"}]`;
    if (v instanceof Node) return v.outerHTML ?? String(v);

    try {
      const text = JSON.stringify(v);
      // `{}` for an object that plainly has contents means the contents were
      // not enumerable; say what it is rather than showing an empty shape.
      if (text === "{}" ) {
        const name = v.constructor?.name;
        return name && name !== "Object" ? `[${name}]` : String(v);
      }
      return text ?? String(v);
    } catch (_) {
      return String(v);
    }
  }

  // ── timers ───────────────────────────────────────────────────────────────
  //
  // The queue lives here and the host drains it, so "has this page settled"
  // is a question with an answer rather than a guess about wall-clock time.

  let nextTimer = 1;
  const timers = new Map();
  let clock = 0;

  // How long a chain of timers that arm each other goes on holding the page open.
  const NESTING_LIMIT = 10;
  let timerDepth = 0;

  function setTimeout(fn, delay, ...args) {
    const id = nextTimer++;
    timers.set(id, {
      fn, due: clock + Math.max(0, delay | 0), args, every: null, depth: timerDepth + 1,
    });
    return id;
  }
  function setInterval(fn, delay, ...args) {
    const id = nextTimer++;
    const every = Math.max(1, delay | 0);
    timers.set(id, { fn, due: clock + every, args, every, depth: timerDepth + 1 });
    return id;
  }
  function clearTimeout(id) { timers.delete(id); }

  // A timer is *blocking* while the page still owes it: one-shot, and not so
  // deep in a self-arming chain that it has stopped converging.
  function timerBlocks(timer) {
    return timer.every === null && timer.depth < NESTING_LIMIT;
  }

  // Returns the number of callbacks run. The host calls this until it returns
  // zero, advancing its own clock, which is what makes a timer chain settle
  // deterministically instead of racing a real one.
  globalThis.__h5iRunTimers = function (now) {
    clock = now;
    let ran = 0;
    for (const [id, timer] of [...timers.entries()].sort((a, b) => a[1].due - b[1].due)) {
      if (timer.due > clock) continue;
      if (timer.every === null) timers.delete(id);
      else timer.due = clock + timer.every;
      // Anything this callback arms inherits its depth, which is what makes a
      // self-arming chain measurable at all. Restored in `finally` so a timer
      // that throws does not leave every later one counted as nested.
      const outer = timerDepth;
      timerDepth = timer.depth;
      try { timer.fn(...timer.args); } catch (error) {
        console.error("timer threw: " + withStack(error));
      } finally {
        timerDepth = outer;
      }
      ran++;
    }
    return ran;
  };

  // Only *converging* timers count as work outstanding.
  lazyGlobals("sockets", ["WebSocket", "EventSource"]);

  /// What the settle loop asks every round. A page that never opened a socket
  /// has no sockets to drain, and answering that without loading the tier is
  /// the whole point: `prelude/sockets.js` replaces both of these when it
  /// arrives.
  globalThis.__h5iDrainSockets = function () { return 0; };
  globalThis.__h5iOpenSockets = function () { return 0; };

  globalThis.__h5iPendingTimers = function () {
    let pending = 0;
    for (const timer of timers.values()) if (timerBlocks(timer)) pending++;
    return pending;
  };

  /// Timers that are armed but no longer hold the page open: intervals, and
  /// one-shots deep enough in a self-arming chain to have stopped converging.
  ///
  /// Reported rather than hidden. "Nothing is left to run" and "the only thing
  /// left is a loop that will never stop" are different answers, and a caller
  /// that waited for an element deserves to know which one it got.
  globalThis.__h5iPeriodicTimers = function () {
    let periodic = 0;
    for (const timer of timers.values()) if (!timerBlocks(timer)) periodic++;
    return periodic;
  };

  /// When the earliest waiting timer is due, or -1 if none is.
  ///
  /// The settle loop uses this to jump the virtual clock to the next thing that
  /// will actually happen, rather than stepping toward it 16ms at a time. A
  /// page that sets one ten-second timeout should cost one step, not six
  /// hundred and twenty-five, and stepping was not merely slow: it meant a
  /// timer due at the settle budget was never reached at all.
  globalThis.__h5iNextTimerDue = function () {
    let soonest = -1;
    for (const timer of timers.values()) {
      if (soonest < 0 || timer.due < soonest) soonest = timer.due;
    }
    return soonest;
  };

  defineLive({
    innerWidth: () => api.viewport().width,
    innerHeight: () => api.viewport().height,
    outerWidth: () => api.viewport().width,
    outerHeight: () => api.viewport().height,
    scrollX: () => document.documentElement.scrollLeft,
    scrollY: () => document.documentElement.scrollTop,
    pageXOffset: () => document.documentElement.scrollLeft,
    pageYOffset: () => document.documentElement.scrollTop,
  });

  // How the host reaches a node it knows only by id, to fire a real event at
  // it. Exposed rather than reimplemented on the Rust side so a synthetic
  // click takes exactly the path a page's own `.click()` takes.
  globalThis.__h5iWrapById = wrap;

  /// How far through loading the document says it is.
  ///
  /// This was the constant `"complete"` until WPT was pointed at the engine.
  /// A constant is the answer that makes the *common* idiom work — the one that
  /// reads `readyState === "loading"` and otherwise initialises immediately —
  /// so every page in §8's four corpora took the immediate branch and nothing
  /// looked wrong. What it hid is that the other branch never arrived, because
  /// no lifecycle event was ever fired at all (§11.5.2).
  let documentReadyState = "loading";

  /// Fire the document lifecycle: DOMContentLoaded, then load.
  globalThis.__h5iInstallNamedAccess = function () {
    for (const id of api.queryAll("[id]", 0)) {
      const name = api.getAttr(id, "id");
      if (!name || !/^[A-Za-z_$][\w$]*$/.test(name)) continue;
      if (name in globalThis) continue;
      Object.defineProperty(globalThis, name, {
        configurable: true,
        enumerable: false,
        get() { return wrap(api.query("#" + cssEscapeIdent(name), 0)); },
        set(value) {
          // Enumerable, because what the page has just created is an ordinary
          // global and should behave like one from here on.
          Object.defineProperty(globalThis, name, {
            configurable: true,
            enumerable: true,
            writable: true,
            value,
          });
        },
      });
    }
  };

  /// Enough escaping to put an id back into a selector safely.
  function cssEscapeIdent(name) {
    return name.replace(/[^\w-]/g, (ch) => "\\" + ch);
  }

  globalThis.__h5iFireLifecycle = function () {
    const root = wrap(api.root());
    const at = (event) => { if (root) root.dispatchEvent(event); };

    // Again here: a script that ran during parsing may have added elements
    // with ids, and the handlers about to run will reach for them by name.
    globalThis.__h5iInstallNamedAccess();

    // Before the first event, not after: `<body onload>` is a handler for the
    // load event dispatched three lines below, so compiling it later would be
    // compiling it too late.
    globalThis.__h5iInstallInlineHandlers();

    documentReadyState = "interactive";
    at(new Event("readystatechange"));
    at(new Event("DOMContentLoaded", { bubbles: true }));

    // Where a browser puts them: an image's `load` has fired by the time the
    // window's does, and a `DOMContentLoaded` handler counting them is already
    // listening.
    globalThis.__h5iFireResourceEvents();

    documentReadyState = "complete";
    at(new Event("readystatechange"));
    at(new Event("load"));
    at(new Event("pageshow"));
  };

  // ── the rest of the window ───────────────────────────────────────────────

  // Every part, from the engine's own parser rather than from string surgery — `pathname` came
  // back undefined, and client-side routing is written against exactly these.
  let currentAddress = globalThis.__h5iUrl;

  function locationParts() {
    return api.parseUrl(String(currentAddress), "") || {};
  }
  const location = {
    get href() { return currentAddress; },
    get protocol() { return locationParts().protocol ?? ""; },
    get host() { return locationParts().host ?? ""; },
    get hostname() { return locationParts().hostname ?? ""; },
    get port() { return locationParts().port ?? ""; },
    get pathname() { return locationParts().pathname ?? ""; },
    get search() { return locationParts().search ?? ""; },
    get hash() { return locationParts().hash ?? ""; },
    /// Hash routing, which a great many single-page applications are built on.
    set hash(value) {
      const wanted = String(value);
      const fragment = wanted.startsWith("#") ? wanted : "#" + wanted;
      const before = currentAddress;
      const parts = api.parseUrl(fragment, currentAddress);
      const next = parts ? parts.href : currentAddress;
      if (next === before) return;
      history.pushState(history.state, "", next);
      const event = new Event("hashchange", { bubbles: false });
      event.oldURL = before;
      event.newURL = next;
      dispatch(wrap(api.root()), event);
    },
    get origin() { return locationParts().origin ?? ""; },
    toString() { return currentAddress; },
    assign(u) { api.unsupported("location.assign"); void u; },
    replace(u) { api.unsupported("location.replace"); void u; },
    reload() { api.unsupported("location.reload"); },
  };

  // Client-side routing goes through this, so a stub meant an SPA changed
  // nothing when it navigated. In memory, current entry plus a short list: the
  // page's own router reads `state` and listens for `popstate`, and both work.
  const entries = [{ state: null, url: globalThis.__h5iUrl }];
  let entryAt = 0;

  /// Resolve a pushed URL against the current one, the way a link would be.
  /// A router pushes `/page/2`, and storing that raw leaves an address no
  /// parser can answer questions about.
  function resolveEntry(url) {
    if (url === undefined || url === null || url === "") return entries[entryAt].url;
    const parts = api.parseUrl(String(url), String(currentAddress));
    return parts ? parts.href : String(url);
  }
  const history = {
    get length() { return entries.length; },
    get state() { return entries[entryAt].state ?? null; },
    pushState(state, _title, url) {
      const next = resolveEntry(url);
      entries.length = entryAt + 1;
      entries.push({ state: state ?? null, url: next });
      entryAt = entries.length - 1;
      // The address has to move with the entry, or `location.pathname` keeps
      // answering about the page the router already left — and a router that
      // reads its own route back gets the wrong one.
      currentAddress = next;
    },
    replaceState(state, _title, url) {
      const next = resolveEntry(url);
      entries[entryAt] = { state: state ?? null, url: next };
      currentAddress = next;
    },
    go(delta) {
      const next = entryAt + (delta | 0);
      if (next < 0 || next >= entries.length) return;
      entryAt = next;
      currentAddress = entries[entryAt].url;
      const event = new Event("popstate", { bubbles: false });
      event.state = entries[entryAt].state;
      dispatch(wrap(api.root()), event);
    },
    back() { history.go(-1); },
    forward() { history.go(1); },
  };

  // `now()` returns the *virtual* clock, deliberately: everything else in this
  // engine measures a page's own timeline rather than the wall, and a page that
  // computed a duration from a real clock would get a number about how loaded
  // this machine was.
  const performanceEntries = [];
  const performanceMarks = new Map();
  const performance = {
    now: () => clock,
    timeOrigin: 0,
    mark(name, options) {
      const at = options && typeof options.startTime === "number" ? options.startTime : clock;
      performanceMarks.set(String(name), at);
      const entry = { name: String(name), entryType: "mark", startTime: at, duration: 0 };
      performanceEntries.push(entry);
      return entry;
    },
    measure(name, startOrOptions, endMark) {
      const startName = typeof startOrOptions === "object" && startOrOptions !== null
        ? startOrOptions.start
        : startOrOptions;
      const start = performanceMarks.get(String(startName)) ?? 0;
      const end = endMark === undefined ? clock : (performanceMarks.get(String(endMark)) ?? clock);
      const entry = {
        name: String(name),
        entryType: "measure",
        startTime: start,
        duration: Math.max(0, end - start),
      };
      performanceEntries.push(entry);
      return entry;
    },
    getEntries() { return performanceEntries.slice(); },
    getEntriesByName(name, type) {
      return performanceEntries.filter(
        (e) => e.name === String(name) && (type === undefined || e.entryType === type),
      );
    },
    getEntriesByType(type) {
      return performanceEntries.filter((e) => e.entryType === String(type));
    },
    clearMarks(name) {
      if (name === undefined) performanceMarks.clear();
      else performanceMarks.delete(String(name));
    },
    clearMeasures() {},
    clearResourceTimings() {},
  };

  // Window-level listeners land on the root element, which is where document
  // and window events already propagate to. Without these, `addEventListener`
  // at global scope is simply undefined, and popstate/DOMContentLoaded
  // handlers — which most routers install — throw on the way in.
  function addEventListener(type, handler, options) {
    const root = wrap(api.root());
    if (root) root.addEventListener(type, handler, options);
  }
  function removeEventListener(type, handler) {
    const root = wrap(api.root());
    if (root) root.removeEventListener(type, handler);
  }
  function dispatchEvent(event) {
    const root = wrap(api.root());
    return root ? root.dispatchEvent(event) : true;
  }

  const window = globalThis;
  /// A `Blob` that actually holds its bytes.
  ///
  /// Pages build one to hand to `URL.createObjectURL`, to read back as text, or
  /// to measure. A stub would satisfy the constructor and then lie about `size`,
  /// which is the shape of bug this engine keeps having to remove.
  class Blob {
    constructor(parts, options) {
      // Bytes, not characters: `size` is a byte count, and a blob of "café" is
      // five bytes rather than four. Getting that wrong is the whole reason to
      // store the encoded form.
      const encoder = new TextEncoder();
      const chunks = [];
      for (const part of parts ?? []) {
        if (part instanceof Blob) chunks.push(...part._bytes);
        else if (part instanceof Uint8Array) chunks.push(...part);
        else if (part && part.buffer) chunks.push(...new Uint8Array(part.buffer));
        else chunks.push(...encoder.encode(String(part)));
      }
      this._bytes = chunks;
      this.type = String((options && options.type) || "");
    }
    get size() { return this._bytes.length; }
    text() { return Promise.resolve(new TextDecoder().decode(new Uint8Array(this._bytes))); }
    arrayBuffer() { return Promise.resolve(new Uint8Array(this._bytes).buffer); }
    bytes() { return Promise.resolve(new Uint8Array(this._bytes)); }
    slice(start, end, type) {
      const cut = new Blob([], { type: type ?? this.type });
      cut._bytes = this._bytes.slice(start, end);
      return cut;
    }
  }

  class File extends Blob {
    constructor(parts, name, options) {
      super(parts, options);
      this.name = String(name);
      this.lastModified = 0;
    }
  }

  /// The error type the platform throws, as distinct from a plain `Error`.
  ///
  /// Libraries construct it (`new DOMException('aborted', 'AbortError')`) and
  /// branch on `.name`, and an abort path that cannot build its own error
  /// throws a `ReferenceError` instead — which is how excalidraw's bundle died
  /// before rendering anything.
  class DOMException extends Error {
    constructor(message, name) {
      super(String(message ?? ""));
      this.name = String(name ?? "Error");
    }
    // The legacy numeric codes, which older code still compares against.
    get code() {
      return {
        IndexSizeError: 1, HierarchyRequestError: 3, WrongDocumentError: 4,
        InvalidCharacterError: 5, NoModificationAllowedError: 7, NotFoundError: 8,
        NotSupportedError: 9, InvalidStateError: 11, SyntaxError: 12,
        InvalidModificationError: 13, NamespaceError: 14, InvalidAccessError: 15,
        SecurityError: 18, NetworkError: 19, AbortError: 20, TimeoutError: 23,
        DataCloneError: 25,
      }[this.name] ?? 0;
    }
  }

  // A constructable stylesheet, backed by a real `<style>` element.
  //
  // Design systems build one, fill it with `replaceSync`, and adopt it — and a
  // page that cannot construct one throws before rendering anything. Backing it
  // with a `<style>` in the head means the rules actually reach Stylo, so
  // `display: none` still hides things from the outline, which is the part that
  // changes what an agent reads.
  //
  // `cssRules` is deliberately **not** defined. This engine does not model rules
  // individually, and answering an empty list for a sheet that plainly has rules
  // would be the confident wrong answer it keeps having to refuse — so it goes
  // unanswered, and reports itself.
  /// Split a stylesheet into its top-level rules.
  ///
  /// Brace matching that knows about strings and comments, which is all
  /// `cssRules` needs: where each rule starts and ends. **It is not a CSS
  /// parser and does not pretend to be one** — the cascade is Stylo's, this
  /// only reports the text back in the shape the CSSOM asks for. A declaration
  /// this splitter mis-slices would still be applied correctly to the page,
  /// because the page's styles never come through here.
  function splitRules(css) {
    const found = [];
    let depth = 0, start = 0, index = 0, quote = null;
    while (index < css.length) {
      const ch = css[index];
      if (quote) {
        if (ch === "\\") index++;
        else if (ch === quote) quote = null;
      } else if (ch === '"' || ch === "'") {
        quote = ch;
      } else if (ch === "/" && css[index + 1] === "*") {
        const end = css.indexOf("*/", index + 2);
        index = end < 0 ? css.length : end + 1;
      } else if (ch === "{") {
        depth++;
      } else if (ch === "}") {
        if (--depth <= 0) {
          found.push(css.slice(start, index + 1));
          start = index + 1;
          depth = 0;
        }
      } else if (depth === 0 && ch === ";") {
        // `@import`, `@charset` and friends: a rule with no block at all.
        found.push(css.slice(start, index + 1));
        start = index + 1;
      }
      index++;
    }
    if (start < css.length) found.push(css.slice(start));
    return found.map((text) => text.trim()).filter(Boolean);
  }

  /// Split one rule into its prelude and its body, or null if it has no body.
  function ruleParts(text) {
    let quote = null;
    for (let index = 0; index < text.length; index++) {
      const ch = text[index];
      if (quote) {
        if (ch === "\\") index++;
        else if (ch === quote) quote = null;
      } else if (ch === '"' || ch === "'") {
        quote = ch;
      } else if (ch === "{") {
        const close = text.lastIndexOf("}");
        return { prelude: text.slice(0, index).trim(), body: text.slice(index + 1, close) };
      }
    }
    return null;
  }

  const RULE_TYPES = {
    style: 1, import: 3, media: 4, "font-face": 5, page: 6, keyframes: 7,
    namespace: 10, supports: 12, "counter-style": 11, "font-feature-values": 14,
    layer: 0, container: 0, property: 0, scope: 0, starting: 0,
  };

  class CSSRule {
    constructor(text, sheet) { this._text = text; this._sheet = sheet; }
    get cssText() { return this._text; }
    /// Push this rule's new text back into the stylesheet it belongs to.
    ///
    /// Without this a mutation was a silent no-op: `cssRules` built a fresh
    /// object per access, so `sheet.cssRules[0].style.color = "blue"` wrote to
    /// a throwaway and the sheet still said red — success reported, nothing
    /// changed. CSS-in-JS libraries mutate rules, so this was the worst kind of
    /// wrong: quiet.
    _changed() { if (this._sheet) this._sheet._rewriteFromRules(); }
    get parentStyleSheet() { return this._sheet ?? null; }
    get parentRule() { return null; }
    get type() {
      const parts = ruleParts(this._text) ?? { prelude: this._text };
      if (!parts.prelude.startsWith("@")) return RULE_TYPES.style;
      const name = parts.prelude.slice(1).split(/[\s({]/)[0].toLowerCase();
      return RULE_TYPES[name] ?? 0;
    }
  }

  class CSSStyleRule extends CSSRule {
    get selectorText() { return (ruleParts(this._text)?.prelude ?? "").trim(); }
    set selectorText(value) {
      const parts = ruleParts(this._text);
      if (!parts) return;
      this._text = `${String(value)} {${parts.body}}`;
      this._changed();
    }
    get style() {
      const rule = this;
      return new StyleDeclaration({
        get: () => (ruleParts(rule._text)?.body ?? "").trim(),
        set: (text) => {
          const parts = ruleParts(rule._text);
          if (!parts) return;
          rule._text = `${parts.prelude} { ${text} }`;
          rule._changed();
        },
      });
    }
  }

  /// `@media`, `@supports` and the other rules that contain rules.
  class CSSGroupingRule extends CSSRule {
    get conditionText() { return (ruleParts(this._text)?.prelude ?? "").replace(/^@\w+\s*/, "").trim(); }
    get cssRules() {
      return splitRules(ruleParts(this._text)?.body ?? "").map((text) => makeRule(text, this._sheet));
    }
  }

  function makeRule(text, sheet) {
    const parts = ruleParts(text);
    if (!parts) return new CSSRule(text, sheet);
    if (!parts.prelude.startsWith("@")) return new CSSStyleRule(text, sheet);
    const name = parts.prelude.slice(1).split(/[\s({]/)[0].toLowerCase();
    if (name === "media" || name === "supports" || name === "container"
      || name === "layer" || name === "scope") {
      return new CSSGroupingRule(text, sheet);
    }
    return new CSSRule(text, sheet);
  }

  /// A stylesheet, either constructed by script or belonging to an element.
  ///
  /// Both directions matter and they are not the same object. A constructed
  /// sheet (`new CSSStyleSheet()`, for `adoptedStyleSheets`) *writes* a
  /// `<style>` element into the document. An element's own sheet — `<style>` or
  /// `<link rel=stylesheet>` — *reads* what is already there. Until WPT asked,
  /// only the first existed, so `document.styleSheets` and `el.sheet` were the
  /// two most-wanted CSSOM gaps on the list at 3,779 calls between them.
  class CSSStyleSheet {
    constructor(options) {
      this._text = "";
      this._element = null;
      this._owned = false;
      this.disabled = !!(options && options.disabled);
      if (options && options.media) this._media = String(options.media);
    }

    /// The sheet an element owns, cached on the element so two reads of
    /// `el.sheet` are the same object, as they are in a browser.
    static forElement(element) {
      if (element._sheet) return element._sheet;
      const sheet = new CSSStyleSheet();
      sheet._element = element;
      sheet._owned = true;
      element._sheet = sheet;
      return sheet;
    }

    get ownerNode() { return this._element; }
    get ownerRule() { return null; }
    get parentStyleSheet() { return null; }
    get type() { return "text/css"; }
    get title() {
      const raw = this._element ? api.getAttr(this._element._id, "title") : null;
      return raw || null;
    }
    /// A `MediaList`, not a string.
    get media() {
      const text = this._media !== undefined
        ? this._media
        : (this._element && api.getAttr(this._element._id, "media")) || "";
      if (this._mediaList === undefined || this._mediaFor !== text) {
        // `sheet` rather than `this`: the write-back closure sits two arrows
        // deep inside a class getter, and Boa does not carry the getter's
        // `this` that far — it arrived `undefined` and every `appendMedium`
        // threw. Naming the receiver is right regardless of whose bug that is.
        const sheet = this;
        this._mediaFor = text;
        this._mediaList = internal(() => new MediaList(text, (written) => {
          sheet._mediaFor = written;
          if (sheet._media !== undefined) sheet._media = written;
          else if (sheet._element) sheet._element.setAttribute("media", written);
        }));
      }
      return this._mediaList;
    }
    /// Null for a `<style>` and for a constructed sheet, as in a browser: only
    /// a sheet that came from a URL has one.
    get href() {
      if (!this._element || this._element.tagName !== "LINK") return null;
      return this._element.href || null;
    }

    _css() {
      if (!this._owned) return this._text;
      // A `<link>`'s bytes were fetched and parsed natively and never reach
      // script, so its rules are not readable here. Empty rather than wrong,
      // and the same answer a browser gives for a cross-origin sheet.
      if (this._element.tagName === "LINK") return "";
      return this._element.textContent || "";
    }

    /// The rules, cached against the text they were parsed from.
    ///
    /// Two reasons, and neither is only speed. A browser's `cssRules` hands
    /// back the same object every time, so a page that keeps a rule and mutates
    /// it later must keep hold of something real; and re-splitting on every
    /// index made a loop over the rules quadratic in the size of the sheet.
    get cssRules() {
      const css = this._css();
      if (this._rulesFor !== css) {
        this._rulesFor = css;
        this._rules = collection(
          splitRules(css).map((text) => makeRule(text, this)), "CSSRuleList",
        );
      }
      return this._rules;
    }
    get rules() { return this.cssRules; }

    /// Re-serialise the cached rules after one of them changed.
    ///
    /// `_rulesFor` is set to the text we just wrote, so the next read finds the
    /// cache warm and the caller keeps the rule object it is holding.
    _rewriteFromRules() {
      if (!this._rules) return;
      const text = this._rules.map((rule) => rule.cssText).join("\n");
      this._rulesFor = text;
      this._replaceAll(text);
    }

    replaceSync(text) {
      this._text = String(text);
      this._apply();
    }
    replace(text) {
      this.replaceSync(text);
      return Promise.resolve(this);
    }
    insertRule(rule, index) {
      const rules = splitRules(this._css());
      const at = index === undefined ? 0 : Math.min(Number(index) || 0, rules.length);
      rules.splice(at, 0, String(rule));
      this._replaceAll(rules.join("\n"));
      return at;
    }
    /// The legacy pair, which is how a great deal of older code still edits a
    /// sheet. Both are defined in CSSOM in terms of the modern two, so they are
    /// written that way rather than reimplemented: `addRule` appends by default
    /// and answers -1, which is the one part that is not just a forward.
    addRule(selector, block, index) {
      const at = index === undefined ? this.cssRules.length : Number(index);
      this.insertRule(`${String(selector || "")} { ${String(block || "")} }`, at);
      return -1;
    }
    removeRule(index) { this.deleteRule(index === undefined ? 0 : index); }
    deleteRule(index) {
      const rules = splitRules(this._css());
      const at = Number(index) || 0;
      if (at < 0 || at >= rules.length) {
        throw new DOMException(`there is no rule ${at} to delete`, "IndexSizeError");
      }
      rules.splice(at, 1);
      this._replaceAll(rules.join("\n"));
    }
    _replaceAll(text) {
      if (this._owned) {
        // A `<link>`'s bytes were fetched and parsed natively and never reach
        // script, so there is nothing here to edit. Refused loudly rather than
        // quietly: `insertRule` used to answer 0 and change nothing, which is a
        // page believing it had installed a style.
        if (this._element.tagName === "LINK") {
          throw new DOMException(
            "this stylesheet came from a <link> and its rules are not editable here",
            "NoModificationAllowedError",
          );
        }
        this._element.textContent = text;
        return;
      }
      this._text = text;
      this._apply();
    }
    _apply() {
      if (!this._element) {
        const head = wrap(api.query("head", 0)) || document.body;
        if (!head) return;
        this._element = document.createElement("style");
        head.appendChild(this._element);
      }
      this._element.textContent = this._text;
    }
  }

  /// The media a sheet or an `@media` rule applies to.
  ///
  /// CSSOM makes this an object over a comma-separated list, and this engine
  /// answered the raw string. Serialising is the whole of it: the list is the
  /// text split on commas and trimmed, and `mediaText` is the list joined back
  /// with ", " — which is why round-tripping normalises whitespace, as it does
  /// in a browser.
  const splitMedia = (text) =>
    String(text ?? "").split(",").map((m) => m.trim()).filter(Boolean);

  class MediaList {
    constructor(text, onWrite) {
      refuseExternal("MediaList");
      this._items = splitMedia(text);
      this._onWrite = onWrite ?? null;
    }
    _changed() { if (this._onWrite) this._onWrite(this.mediaText); }
    get mediaText() { return this._items.join(", "); }
    set mediaText(value) { this._items = splitMedia(value); this._changed(); }
    get length() { return this._items.length; }
    item(index) { return this._items[Number(index) || 0] ?? null; }
    appendMedium(medium) {
      const one = String(medium).trim();
      if (one && !this._items.includes(one)) { this._items.push(one); this._changed(); }
    }
    deleteMedium(medium) {
      const at = this._items.indexOf(String(medium).trim());
      if (at === -1) throw new DOMException(`no medium ${medium}`, "NotFoundError");
      this._items.splice(at, 1);
      this._changed();
    }
    toString() { return this.mediaText; }
  }

  /// Every sheet the document owns, in document order.
  ///
  /// Indexed as well as iterable, because `document.styleSheets[0]` is how
  /// almost every caller reaches for it.
  function styleSheetList() {
    const sheets = api
      .queryAll("style, link[rel~=stylesheet i]", 0)
      .map(wrap)
      .filter(Boolean)
      .map((element) => CSSStyleSheet.forElement(element));
    return collection(sheets, "StyleSheetList");
  }

  // ── text encoding, randomness, cloning, and the old request object ───────

  // UTF-8, written out rather than approximated. `escape`/`unescape` round
  // trips and `charCodeAt` truncation both get the common cases right and the
  // rest wrong, and "wrong only for non-Latin text" is the failure mode this
  // engine is least able to notice.
  class TextEncoder {
    get encoding() { return "utf-8"; }
    encode(input) {
      const text = String(input === undefined ? "" : input);
      const out = [];
      for (let i = 0; i < text.length; i++) {
        let code = text.codePointAt(i);
        if (code > 0xffff) i++; // a surrogate pair is one code point
        if (code < 0x80) {
          out.push(code);
        } else if (code < 0x800) {
          out.push(0xc0 | (code >> 6), 0x80 | (code & 63));
        } else if (code < 0x10000) {
          out.push(0xe0 | (code >> 12), 0x80 | ((code >> 6) & 63), 0x80 | (code & 63));
        } else {
          out.push(
            0xf0 | (code >> 18),
            0x80 | ((code >> 12) & 63),
            0x80 | ((code >> 6) & 63),
            0x80 | (code & 63),
          );
        }
      }
      return new Uint8Array(out);
    }
  }

  class TextDecoder {
    /// Validates its label, and decodes as *that* label.
    ///
    /// Both used to be wrong rather than missing: every label was accepted and
    /// every one answered "utf-8", so a page asking whether an encoding was
    /// supported was told yes and a page decoding Shift-JIS got mojibake with
    /// no error. The label table and the decoders are `encoding_rs`'s, which is
    /// the same table the standard defines rather than a list of our own that
    /// would drift from it.
    constructor(label, options) {
      const wanted = label === undefined ? "utf-8" : String(label);
      const canonical = api.encodingFor(wanted);
      if (canonical === null || canonical === undefined) {
        throw new RangeError(`${wanted} is not a known encoding`);
      }
      // `replacement` exists only to be refused, and decoding as it is not a
      // thing a caller can ask for.
      this._encoding = canonical;
      this._fatal = !!(options && options.fatal);
      this._ignoreBOM = !!(options && options.ignoreBOM);
    }
    get encoding() { return this._encoding; }
    get fatal() { return this._fatal; }
    get ignoreBOM() { return this._ignoreBOM; }
    decode(input) {
      if (input === undefined || input === null) return "";
      // Anything byte-shaped: a typed array, an ArrayBuffer, or a plain array.
      const bytes = input instanceof Uint8Array
        ? input
        : new Uint8Array(input.buffer ? input.buffer : input);
      return api.decodeBytes(this._encoding, Array.from(bytes), this._fatal);
    }
  }

  const crypto = {
    getRandomValues(target) {
      if (!target || typeof target.length !== "number") {
        throw new TypeError("getRandomValues expects a typed array");
      }
      // Per element, not per byte: the caller's array decides the width.
      const width = target.BYTES_PER_ELEMENT || 1;
      const bytes = api.randomBytes(target.length * width);
      for (let i = 0; i < target.length; i++) {
        let value = 0;
        for (let b = 0; b < width; b++) value = value * 256 + bytes[i * width + b];
        target[i] = value;
      }
      return target;
    },
    randomUUID() {
      const bytes = api.randomBytes(16);
      // Version 4, variant 1 — the two fields a v4 UUID is defined by.
      bytes[6] = (bytes[6] & 0x0f) | 0x40;
      bytes[8] = (bytes[8] & 0x3f) | 0x80;
      const hex = bytes.map((b) => b.toString(16).padStart(2, "0"));
      return [
        hex.slice(0, 4).join(""),
        hex.slice(4, 6).join(""),
        hex.slice(6, 8).join(""),
        hex.slice(8, 10).join(""),
        hex.slice(10, 16).join(""),
      ].join("-");
    },
  };

  // A real deep clone. The JSON round trip this replaces silently dropped
  // `undefined`, turned a `Date` into a string, lost `Map` and `Set` entirely,
  // and threw on a cycle — every one of which reads as the page's own bug.
  function structuredClone(value, seen) {
    seen = seen || new Map();
    if (value === null || typeof value !== "object") return value;
    if (seen.has(value)) return seen.get(value);

    if (value instanceof Date) return new Date(value.getTime());
    if (value instanceof RegExp) return new RegExp(value.source, value.flags);
    if (Array.isArray(value)) {
      const out = [];
      seen.set(value, out);
      for (const item of value) out.push(structuredClone(item, seen));
      return out;
    }
    if (value instanceof Map) {
      const out = new Map();
      seen.set(value, out);
      for (const [k, v] of value) out.set(structuredClone(k, seen), structuredClone(v, seen));
      return out;
    }
    if (value instanceof Set) {
      const out = new Set();
      seen.set(value, out);
      for (const item of value) out.add(structuredClone(item, seen));
      return out;
    }
    // A node is not transferable, and pretending otherwise would hand the page
    // a detached copy that silently does nothing.
    if (value instanceof Node) {
      throw new TypeError("structuredClone cannot clone a DOM node");
    }
    const out = {};
    seen.set(value, out);
    for (const [k, v] of Object.entries(value)) out[k] = structuredClone(v, seen);
    return out;
  }

  // The old request object, over the same queue `fetch` uses — so an XHR is
  // policy-checked and receipted identically, and overlaps with everything else
  // in flight. Libraries that predate `fetch` are still everywhere.
  class XMLHttpRequest {
    constructor() {
      this.readyState = 0;
      this.status = 0;
      this.statusText = "";
      this.responseText = "";
      this.response = "";
      this.responseType = "";
      this.onreadystatechange = null;
      this.onload = null;
      this.onerror = null;
      this._method = "GET";
      this._url = "";
      this._headers = new Headers();
      this._responseHeaders = new Headers();
    }
    open(method, url, async) {
      // Synchronous XHR would have to block the one thread that owns the realm,
      // which would deadlock the loop that answers the request. Named rather
      // than silently upgraded to async, because a page relying on the return
      // value would read an empty response as an empty server.
      if (async === false) api.unsupported("XMLHttpRequest (synchronous)");
      this._method = String(method || "GET").toUpperCase();
      this._url = String(url);
      this._transition(1);
    }
    setRequestHeader(name, value) { this._headers.append(name, value); }
    getAllResponseHeaders() {
      let out = "";
      for (const [name, value] of this._responseHeaders) out += `${name}: ${value}\r\n`;
      return out;
    }
    getResponseHeader(name) { return this._responseHeaders.get(name); }
    abort() { this._aborted = true; this._transition(4); }
    _transition(state) {
      this.readyState = state;
      if (typeof this.onreadystatechange === "function") {
        try { this.onreadystatechange(); } catch (e) { console.error(`XHR onreadystatechange threw: ${e}`); }
      }
    }
    send(body) {
      fetch(this._url, { method: this._method, body, headers: this._headers })
        .then((response) => response.text().then((text) => ({ response, text })))
        .then(({ response, text }) => {
          if (this._aborted) return;
          this.status = response.status;
          this.statusText = response.statusText;
          this._responseHeaders = response.headers;
          this.responseText = text;
          this.response = this.responseType === "json" ? JSON.parse(text) : text;
          this._transition(4);
          if (typeof this.onload === "function") this.onload();
        })
        .catch((error) => {
          if (this._aborted) return;
          this.status = 0;
          this._transition(4);
          if (typeof this.onerror === "function") this.onerror(error);
          else console.error(`XMLHttpRequest failed: ${error}`);
        });
    }
  }

  /// The `CSS` namespace: `CSS.escape` and `CSS.supports`.
  ///
  /// `supports` is answered by actually handing the declaration to Stylo rather
  /// than by consulting a list, because a list is a second opinion about what
  /// this engine supports and the two would drift the moment Stylo moved. It
  /// also matters more than most answers: pages call `CSS.supports` in order to
  /// take a *different code path*, so a wrong answer does not degrade the page,
  /// it misroutes it.
  const CSS = observed({
    escape(value) {
      // The spec's algorithm, which is not `encodeURIComponent` and not a
      // regex over "special characters": the rules for a leading digit, a
      // leading hyphen-digit, and NULL are each different, and a selector
      // built from a wrong escape silently matches nothing.
      const text = String(value);
      let out = "";
      for (let index = 0; index < text.length; index++) {
        const code = text.charCodeAt(index);
        const ch = text[index];
        if (code === 0) { out += "\uFFFD"; continue; }
        if ((code >= 0x1 && code <= 0x1f) || code === 0x7f
          || (index === 0 && code >= 0x30 && code <= 0x39)
          || (index === 1 && code >= 0x30 && code <= 0x39 && text.charCodeAt(0) === 0x2d)) {
          out += "\\" + code.toString(16) + " ";
          continue;
        }
        if (index === 0 && code === 0x2d && text.length === 1) { out += "\\" + ch; continue; }
        if (code >= 0x80 || code === 0x2d || code === 0x5f
          || (code >= 0x30 && code <= 0x39) || (code >= 0x41 && code <= 0x5a)
          || (code >= 0x61 && code <= 0x7a)) {
          out += ch;
          continue;
        }
        out += "\\" + ch;
      }
      return out;
    },
    supports(propertyOrCondition, value) {
      if (value !== undefined) {
        return api.supportsCss(String(propertyOrCondition), String(value));
      }
      // The one-argument form takes a condition, `(display: grid)`. Only the
      // plain parenthesised declaration is answered; `and`/`or`/`not` are a
      // grammar rather than a declaration and are named rather than guessed.
      const text = String(propertyOrCondition).trim();
      const match = /^\(\s*([-\w]+)\s*:\s*([^]*?)\s*\)$/.exec(text);
      if (!match) {
        api.unsupported(`CSS.supports(${text.slice(0, 40)})`);
        return false;
      }
      return api.supportsCss(match[1], match[2]);
    },
  }, "CSS");

  /// `new Document()`, which the DOM genuinely defines.
  ///
  /// Separate from `brand` because the brand's contract is "this is not
  /// constructible", and for `Document` that would be a lie with teeth: see the
  /// comment at its call site.
  function documentConstructor(name) {
    const ctor = function () {
      return new DOMParser().parseFromString("", "text/html");
    };
    Object.defineProperty(ctor, "name", { value: name });
    Object.defineProperty(ctor, "prototype", {
      value: ctor.prototype, writable: false, enumerable: false, configurable: false,
    });
    Object.defineProperty(ctor, Symbol.hasInstance, {
      value: (value) => {
        try {
          return !!value && value.nodeType === 9;
        } catch {
          return false;
        }
      },
    });
    // The live document's whole surface, mirrored onto the prototype as
    // forwarding members. Nothing real inherits from this prototype — the
    // constructor above returns a parsed document with its own properties —
    // so the forwarding is only ever reached by code inspecting the
    // *interface*, which is exactly idlharness asking "does Document.prototype
    // have `body`". One document per engine makes the forward unambiguous.
    for (const [key, d] of Object.entries(Object.getOwnPropertyDescriptors(documentImpl))) {
      if (key.startsWith("_") || key === "constructor") continue;
      const forwarded = { configurable: true, enumerable: true };
      if (d.get || d.set) {
        if (d.get) {
          forwarded.get = function () { return document[key]; };
          Object.defineProperty(forwarded.get, "name", { value: `get ${key}` });
        }
        if (d.set) {
          forwarded.set = function (value) { document[key] = value; };
          Object.defineProperty(forwarded.set, "name", { value: `set ${key}` });
        }
      } else if (typeof d.value === "function") {
        forwarded.writable = true;
        forwarded.value = function (...args) { return documentImpl[key](...args); };
        Object.defineProperty(forwarded.value, "name", { value: key });
      } else {
        forwarded.get = function () { return document[key]; };
        forwarded.set = function (value) { document[key] = value; };
        Object.defineProperty(forwarded.get, "name", { value: `get ${key}` });
        Object.defineProperty(forwarded.set, "name", { value: `set ${key}` });
      }
      Object.defineProperty(ctor.prototype, key, forwarded);
    }
    return ctor;
  }

  /// A legacy factory constructor: `new Image(w, h)` and friends.
  ///
  /// Positional arguments map onto properties in the order the spec gives, so
  /// `new Option("label", "value")` sets the text and the value — which is the
  /// whole reason anyone still writes it.
  function makeElementFactory(tag, positional) {
    const ctor = function (...args) {
      const element = document.createElement(tag);
      for (let i = 0; i < positional.length && i < args.length; i++) {
        if (args[i] === undefined) continue;
        element[positional[i]] = args[i];
      }
      return element;
    };
    Object.defineProperty(ctor, "name", { value: `HTML${tag}` });
    return ctor;
  }

  /// Interface objects for shapes this engine builds without a class.
  function interfaceObjects() {
    const brand = (name, test) => {
      const ctor = function () {
        throw new TypeError(`Illegal constructor: ${name} is not constructible`);
      };
      Object.defineProperty(ctor, "name", { value: name });
      // WebIDL §3.7.1: an interface object's `prototype` is
      // { writable: false, enumerable: false, configurable: false }. A plain
      // function's is writable, which `idlharness` checks on its second
      // assertion for every interface — so getting this wrong costs a subtest
      // per interface before anything about the interface is examined.
      Object.defineProperty(ctor, "prototype", {
        value: ctor.prototype,
        writable: false,
        enumerable: false,
        configurable: false,
      });
      Object.defineProperty(ctor, Symbol.hasInstance, {
        value: (value) => {
          try {
            return !!value && test(value);
          } catch {
            return false;
          }
        },
      });
      return ctor;
    };

    // A live-ish list: `collection()` hands back an array carrying `item` and
    // `namedItem`, so those two plus array-ness are the brand.
    const isCollection = (v) => Array.isArray(v) && typeof v.item === "function";
    const isStorage = (v) =>
      typeof v === "object" && typeof v.getItem === "function" &&
      typeof v.setItem === "function" && typeof v.key === "function";

    return {
      NodeList: COLLECTION_CLASSES.NodeList,
      HTMLCollection: COLLECTION_CLASSES.HTMLCollection,
      HTMLFormControlsCollection: COLLECTION_CLASSES.HTMLFormControlsCollection,
      HTMLOptionsCollection: COLLECTION_CLASSES.HTMLOptionsCollection,
      FileList: COLLECTION_CLASSES.FileList,
      CSSRuleList: COLLECTION_CLASSES.CSSRuleList,
      StyleSheetList: COLLECTION_CLASSES.StyleSheetList,
      NamedNodeMap: brand("NamedNodeMap", isCollection),
      Storage: brand("Storage", isStorage),
      // **Constructible, unlike its neighbours here.** `new Document()` is legal — DOM §4.5
      // gives Document a constructor — and getting that wrong is not a small error:
      // `html/dom/idlharness` builds one in its setup, so a `Document` that threw took the
      // whole file from 269 passing subtests to reporting nothing at all.
      Document: documentConstructor("Document"),
      HTMLDocument: documentConstructor("HTMLDocument"),
      DocumentType: brand("DocumentType", (v) => v && v.nodeType === 10),
      ProcessingInstruction: brand("ProcessingInstruction", (v) => v && v.nodeType === 7),
      Attr,
      Window: brand("Window", (v) => v === globalThis),
      Navigator: brand("Navigator", (v) => v && typeof v.userAgent === "string"),
      Location: brand("Location", (v) => v && typeof v.href === "string" && typeof v.assign === "function"),
      History: brand("History", (v) => v && typeof v.pushState === "function"),
      Performance: brand("Performance", (v) => v && typeof v.now === "function"),
      DOMImplementation: brand("DOMImplementation", (v) => v && typeof v.createHTMLDocument === "function"),
      CustomElementRegistry: brand("CustomElementRegistry", (v) => v && typeof v.define === "function" && typeof v.get === "function"),
      DOMStringMap: brand("DOMStringMap", (v) => typeof v === "object"),
      StyleSheet: brand("StyleSheet", (v) => v && "cssRules" in v),
      MediaList,
    };
  }

  // ── data-shape interfaces ────────────────────────────────────────────────
  //
  // Interfaces that are *shapes over data this engine really has* — a media
  // error code, an empty plugin list, pixel bytes — as opposed to the
  // capability interfaces (Worker, Navigation, Sanitizer) that stay absent
  // deliberately: declaring one of those would send feature detection down a
  // branch whose machinery does not exist, which is the lie this engine
  // refuses everywhere. An empty PluginArray is what a plugin-less browser
  // shows; a Worker that cannot run is not what a worker-less page expects.
  class MediaError {
    constructor() { throw new TypeError("Illegal constructor"); }
    get code() {
      if (!this || this.__h5iCode === undefined) {
        throw new TypeError("Illegal invocation: code needs a MediaError");
      }
      return this.__h5iCode;
    }
    get message() { return ""; }
  }
  Object.defineProperty(MediaError.prototype, Symbol.toStringTag, {
    value: "MediaError", configurable: true,
  });
  class TimeRanges {
    constructor() { throw new TypeError("Illegal constructor"); }
    get length() { return 0; }
    start() { throw new DOMException("no ranges", "IndexSizeError"); }
    end() { throw new DOMException("no ranges", "IndexSizeError"); }
  }
  class DOMStringList {
    constructor() { throw new TypeError("Illegal constructor"); }
    get length() { return 0; }
    item() { return null; }
    contains() { return false; }
  }
  /// One bit of browser chrome, reporting whether it is on screen.
  ///
  /// A data shape rather than a capability: there is no chrome here, so every
  /// bar answers `false`, and that is the true answer rather than a stub —
  /// `window.toolbar.visible` is false in a headless browser because the
  /// toolbar is genuinely not visible. The brand guard is what makes reading
  /// `visible` off the prototype throw, which is what idlharness asks.
  class BarProp {
    constructor() { throw new TypeError("Illegal constructor"); }
    get visible() {
      if (!(this instanceof BarProp) || this === BarProp.prototype) {
        throw new TypeError("Illegal invocation: visible needs a BarProp");
      }
      return false;
    }
  }
  Object.defineProperty(BarProp.prototype, Symbol.toStringTag, {
    value: "BarProp", configurable: true,
  });
  class ImageData {
    constructor(dataOrWidth, widthOrHeight, height) {
      if (dataOrWidth instanceof Uint8ClampedArray) {
        this.data = dataOrWidth;
        this.width = widthOrHeight;
        this.height = height ?? (dataOrWidth.length / 4 / widthOrHeight);
      } else {
        this.width = Number(dataOrWidth);
        this.height = Number(widthOrHeight);
        if (!(this.width > 0) || !(this.height > 0)) {
          throw new DOMException("ImageData: zero dimensions", "IndexSizeError");
        }
        this.data = new Uint8ClampedArray(this.width * this.height * 4);
      }
      this.colorSpace = "srgb";
    }
  }
  class DataTransferItemList {
    constructor() { throw new TypeError("Illegal constructor"); }
    get length() { return 0; }
    add() { return null; }
    remove() {}
    clear() {}
  }
  class DataTransferItem {
    constructor() { throw new TypeError("Illegal constructor"); }
  }
  class DataTransfer {
    constructor() {
      this.dropEffect = "none";
      this.effectAllowed = "none";
      this.types = [];
      this.files = collection([], "FileList");
      this.items = Object.create(DataTransferItemList.prototype);
      this.__h5iData = new Map();
    }
    setData(format, data) { this.__h5iData.set(String(format), String(data)); }
    getData(format) { return this.__h5iData.get(String(format)) ?? ""; }
    clearData(format) {
      if (format === undefined) this.__h5iData.clear();
      else this.__h5iData.delete(String(format));
    }
    setDragImage() {}
  }
  class Plugin { constructor() { throw new TypeError("Illegal constructor"); } }
  class PluginArray {
    constructor() { throw new TypeError("Illegal constructor"); }
    get length() { return 0; }
    item() { return null; }
    namedItem() { return null; }
    refresh() {}
  }
  class MimeType { constructor() { throw new TypeError("Illegal constructor"); } }
  class MimeTypeArray {
    constructor() { throw new TypeError("Illegal constructor"); }
    get length() { return 0; }
    item() { return null; }
    namedItem() { return null; }
  }
  class RadioNodeList extends COLLECTION_CLASSES.NodeList {}
  class HTMLAllCollection {
    constructor() { throw new TypeError("Illegal constructor"); }
    item() { return null; }
    namedItem() { return null; }
  }
  class TextMetrics {
    constructor() { throw new TypeError("Illegal constructor"); }
  }
  Object.assign(globalThis, {
    MediaError, TimeRanges, DOMStringList, ImageData, DataTransfer, BarProp,
    MediaQueryList,
    DataTransferItem, DataTransferItemList, Plugin, PluginArray, MimeType,
    MimeTypeArray, RadioNodeList, HTMLAllCollection, TextMetrics,
  });
  {
    const CONSTS = {
      MEDIA_ERR_ABORTED: 1, MEDIA_ERR_NETWORK: 2,
      MEDIA_ERR_DECODE: 3, MEDIA_ERR_SRC_NOT_SUPPORTED: 4,
    };
    for (const target of [MediaError, MediaError.prototype]) {
      for (const [name, value] of Object.entries(CONSTS)) {
        Object.defineProperty(target, name, {
          value, writable: false, enumerable: true, configurable: false,
        });
      }
    }
  }

  // Every `<script>` the parser delivered belongs to the Rust runner, which
  // collected them before any code ran. Marked here so the script-inserted
  // path above never mistakes one for new — inserting a fragment *near* an
  // old script must not run that script twice.
  for (const id of api.queryAll("script", 0)) {
    const el = wrap(id);
    if (el) el.__h5iScriptStarted = true;
  }

  // The one way to arm user activation from outside: the testdriver shim's
  // click calls this, standing in for the user it simulates. Non-enumerable,
  // so pages walking `window` never meet it.
  Object.defineProperty(globalThis, "__h5iNoteUserActivation", {
    value: () => { userActivation.active = true; userActivation.hasBeen = true; },
    writable: false, enumerable: false, configurable: false,
  });

  // ── WebIDL constants ─────────────────────────────────────────────────────
  //
  // On the interface object *and* its prototype, as the IDL `const` rules
  // say — which is why `Node.ELEMENT_NODE` and `node.ELEMENT_NODE` both work.
  // Real code leans on the first form constantly (`n.nodeType ===
  // Node.ELEMENT_NODE` is the idiom), and with the constant undefined that
  // comparison is quietly false for every node, which sent whole test files
  // walking past their target element into a null.
  {
    const defineConstants = (Interface, table) => {
      for (const target of [Interface, Interface.prototype]) {
        for (const [name, value] of Object.entries(table)) {
          Object.defineProperty(target, name, {
            value, writable: false, enumerable: true, configurable: false,
          });
        }
      }
    };
    defineConstants(Node, {
      ELEMENT_NODE: 1, ATTRIBUTE_NODE: 2, TEXT_NODE: 3, CDATA_SECTION_NODE: 4,
      ENTITY_REFERENCE_NODE: 5, ENTITY_NODE: 6, PROCESSING_INSTRUCTION_NODE: 7,
      COMMENT_NODE: 8, DOCUMENT_NODE: 9, DOCUMENT_TYPE_NODE: 10,
      DOCUMENT_FRAGMENT_NODE: 11, NOTATION_NODE: 12,
      DOCUMENT_POSITION_DISCONNECTED: 0x01, DOCUMENT_POSITION_PRECEDING: 0x02,
      DOCUMENT_POSITION_FOLLOWING: 0x04, DOCUMENT_POSITION_CONTAINS: 0x08,
      DOCUMENT_POSITION_CONTAINED_BY: 0x10,
      DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC: 0x20,
    });
    defineConstants(Event, {
      NONE: 0, CAPTURING_PHASE: 1, AT_TARGET: 2, BUBBLING_PHASE: 3,
    });
    defineConstants(Range, {
      START_TO_START: 0, START_TO_END: 1, END_TO_END: 2, END_TO_START: 3,
    });
    defineConstants(XMLHttpRequest, {
      UNSENT: 0, OPENED: 1, HEADERS_RECEIVED: 2, LOADING: 3, DONE: 4,
    });
    defineConstants(CSSRule, {
      STYLE_RULE: 1, CHARSET_RULE: 2, IMPORT_RULE: 3, MEDIA_RULE: 4,
      FONT_FACE_RULE: 5, PAGE_RULE: 6, MARGIN_RULE: 9, NAMESPACE_RULE: 10,
      KEYFRAMES_RULE: 7, KEYFRAME_RULE: 8, SUPPORTS_RULE: 12,
    });
    // ── WebIDL member decoration ──────────────────────────────────────────
    //
    // Its own source, parsed only when an instrument asks: see
    // `prelude/conformance.js`. Called from *here* rather than after the
    // prelude finishes because the decoration has to see the prototypes in the
    // state the rest of this file leaves them in, and the interfaces it needs
    // are closure bindings that a separately parsed source cannot reach.
    internals.polishTargets = [
      EventTarget, Node, Element, Text, Comment, CharacterData,
      DocumentFragment, Range, Event, XMLHttpRequest, CSSRule,
    ];
    internals.TAG_CLASSES = TAG_CLASSES;
    if (globalThis.__h5iConformance) __h5iTier("conformance");

    defineConstants(DOMException, {
      INDEX_SIZE_ERR: 1, DOMSTRING_SIZE_ERR: 2, HIERARCHY_REQUEST_ERR: 3,
      WRONG_DOCUMENT_ERR: 4, INVALID_CHARACTER_ERR: 5, NO_DATA_ALLOWED_ERR: 6,
      NO_MODIFICATION_ALLOWED_ERR: 7, NOT_FOUND_ERR: 8, NOT_SUPPORTED_ERR: 9,
      INUSE_ATTRIBUTE_ERR: 10, INVALID_STATE_ERR: 11, SYNTAX_ERR: 12,
      INVALID_MODIFICATION_ERR: 13, NAMESPACE_ERR: 14, INVALID_ACCESS_ERR: 15,
      VALIDATION_ERR: 16, TYPE_MISMATCH_ERR: 17, SECURITY_ERR: 18,
      NETWORK_ERR: 19, ABORT_ERR: 20, URL_MISMATCH_ERR: 21,
      QUOTA_EXCEEDED_ERR: 22, TIMEOUT_ERR: 23, INVALID_NODE_TYPE_ERR: 24,
      DATA_CLONE_ERR: 25,
    });
  }

  // The session's identity: one crossing per realm, before anything below is
  // defined, so every value derived from it is an own property of its object
  // from the moment that object exists.
  //
  // `api.identity` is absent in a build without the `identity` feature, and
  // then these are the answers this engine gave before identities existed. The
  // literal is not a second source of truth that could drift from the first:
  // `identity::native()` is built from the same two constants, and
  // `the_bare_build_answers_what_native_declares` fails the moment they differ.
  const identity = api.identity ? api.identity() : {
    mode: "native",
    platform: "", vendor: "", productSub: "20030107", oscpu: "",
    hardwareConcurrency: 1, maxTouchPoints: 0,
    languages: ["en-US", "en"],
  };

  Object.assign(globalThis, {
    CSS,
    addEventListener, removeEventListener, dispatchEvent,
    window,
    // The browsing context's view of itself. These are not stubs: §6 refuses
    // iframes and popups, so this document is always a top-level context with
    // no children, and every value below is what a real browser reports for
    // one. `self` in particular gates a great deal of library code — the whole
    // of testharness.js walks `w != w.parent` from `self` before it can run a
    // single assertion, so its absence read as "the engine cannot run WPT"
    // rather than as one missing binding.
    getSelection,
    Selection,
    self: window,
    parent: window,
    top: window,
    frames: window,
    length: 0,
    frameElement: null,
    opener: null,

    /// `window.open`: a named refusal carrying the recovery, per §B15.6.
    open(url, target, features) {
      void target; void features;
      const named = url === undefined || url === null || url === "" ? "about:blank" : String(url);
      api.unsupported(`window.open(${named})`);
      console.warn(
        `window.open(${JSON.stringify(named)}) refused: this engine has one page per session. ` +
        `Open it in another session (h5i browser open ${named} --session <name> --new) ` +
        `and drive both.`,
      );
      return null;
    },
    document,
    console,
    // Same reporting rule as `document`: a method missing from one of these was
    // invisible, because only the document and its nodes were watched. A module
    // failing with "not a callable function" and naming nothing is the failure
    // §8.3 exists to prevent, and these are where the remaining ones hid.
    location: observed(location, "location"),
    history: observed(history, "history"),
    performance: observed(performance, "performance"),
    setTimeout, clearTimeout,
    setInterval, clearInterval: clearTimeout,
    requestAnimationFrame: (fn) => setTimeout(() => fn(clock), 16),
    cancelAnimationFrame: clearTimeout,
    Node, Element, Text, Event,
    alert: () => api.unsupported("alert"),

    // ── The Window members a headless, single-context engine can answer
    //    honestly ──────────────────────────────────────────────────────────
    //
    // Absent until now, and every one of them has a *true* answer here rather
    // than a plausible one — which is the only reason they are being added.
    // §B8.4's rule holds: a name that exists and answers wrongly is worse than
    // one that is absent, so anything whose honest answer would be a guess
    // (`visualViewport`) stays out. `screen` was named here too and no longer
    // is: it appears only when the identity declares a display, so its numbers
    // are stated rather than guessed. See the `Screen` class above.
    //
    // There is no browser chrome, so every `BarProp` reports `visible: false`.
    // That is not a stub standing in for a toolbar we failed to build — it is
    // what "no UI" means, and it is what a headless browser reports.
    ...Object.fromEntries(
      ["locationbar", "menubar", "personalbar", "scrollbars", "statusbar", "toolbar"]
        .map((bar) => [bar, Object.create(BarProp.prototype)]),
    ),
    // Legacy, and writable: pages set it and read it back. "" is what a browser
    // that ignores the status bar reports, which is every browser now.
    status: "",
    // This window was never script-opened and cannot be closed, so `closed` is
    // false for the life of the page and `close()` is the spec's no-op rather
    // than a refusal — HTML only closes a window that script opened.
    closed: false,
    close: () => {},
    // Focus follows no pointer and there is no window manager to raise, so
    // these are no-ops in the same sense: the state they would change does not
    // exist. `stop()` is the one with a real meaning, and it is named rather
    // than pretended — halting a load in flight is broker work this does not do.
    focus: () => {},
    blur: () => {},
    // There is no window to move, resize, print or halt; a page that asks is
    // asking for chrome, and gets the named refusal every other chrome verb
    // here gets. Built from the names so each costs a word rather than a line.
    // The two-argument arity is what WebIDL gives `moveTo` and its neighbours;
    // `stop` and `print` take none, and an operation's `length` is checked.
    ...Object.fromEntries(["stop", "print"].map(
      (verb) => [verb, () => api.unsupported(`window.${verb}`)],
    )),
    ...Object.fromEntries(["moveTo", "moveBy", "resizeTo", "resizeBy"].map(
      (verb) => [verb, (a, b) => api.unsupported(`window.${verb}`)],
    )),
    // Real values, computed from the document's own URL rather than declared.
    get origin() {
      try {
        const url = new URL(location.href);
        return url.protocol === "file:" ? "null" : url.origin;
      } catch { return "null"; }
    },
    get isSecureContext() {
      try {
        const url = new URL(location.href);
        return /^(https|file|data|blob):$/.test(url.protocol)
          || /^(localhost|127\.0\.0\.1|\[::1\])$/.test(url.hostname);
      } catch { return false; }
    },
    // One realm, one context, and no cross-origin isolation to claim.
    crossOriginIsolated: false,

    matchMedia,
    URL, URLSearchParams,
    queueMicrotask: (fn) => { Promise.resolve().then(fn); },
    structuredClone: (value) => structuredClone(value),
    requestIdleCallback: (fn) => setTimeout(() => fn({ didTimeout: false, timeRemaining: () => 0 }), 1),
    cancelIdleCallback: clearTimeout,
    navigator: observed({
      // From the host, not a second copy: a page that branches on the agent
      // server-side and again in script must see the same string both times,
      // or it renders for one engine and scripts for another.
      userAgent: api.userAgent(),
      // Every browser answers "Netscape" here, and `appVersion` is the agent
      // string with its product token removed. Both are derived from the one
      // constant rather than written again, so they cannot drift from it.
      appName: "Netscape",
      appVersion: api.userAgent().replace(/^Mozilla\//, ""),
      appCodeName: "Mozilla",
      product: "Gecko",
      // ── The session's identity, from the host ────────────────────────────
      //
      // Literals here were the bug, not the style: the broker wrote the same
      // facts into `User-Agent` and `Accept-Language` from another file, and
      // the two had already drifted — the header offered `en` and this array
      // did not. One identity now, read through `api.identity()`, and defined
      // here rather than patched on afterwards because a `defineProperty` over
      // `navigator` is visible three ways: descriptor, getter `toString`,
      // prototype. See `identity.rs`.
      vendor: identity.vendor,
      platform: identity.platform,
      language: identity.languages[0],
      languages: Object.freeze(identity.languages.slice()),
      onLine: true, cookieEnabled: false,
      maxTouchPoints: identity.maxTouchPoints,
      hardwareConcurrency: identity.hardwareConcurrency,
      productSub: identity.productSub, vendorSub: "", oscpu: identity.oscpu,
      pdfViewerEnabled: false,
      // Empty, which is what a plugin-less browser shows — the interfaces
      // are real, the lists have nothing in them.
      plugins: Object.create(PluginArray.prototype),
      mimeTypes: Object.create(MimeTypeArray.prototype),
      // False, and true: this is not a driven browser in the WebDriver sense.
      // A page fingerprinting for automation gets the same answer a person's
      // browser gives, because the answer is not about who is asking.
      webdriver: false,
      // Live views over the engine's one activation flag (see
      // `userActivation` at the top): reading through the object always
      // answers the current state, which is what "transient" means.
      userActivation: {
        get isActive() { return userActivation.active; },
        get hasBeenActive() { return userActivation.hasBeen; },
      },
      // `userAgentData` and `scheduling` are deliberately *not* declared here.
      // Writing `userAgentData: undefined` would make `'userAgentData' in
      // navigator` answer true, which is the same lie the `missingApi` stubs
      // told: a page checking before using would take the branch for an API
      // that is not there. Left absent, they behave as they do in Firefox, and
      // the reporting proxy still names them.
    }, "navigator"),
    // Named rather than absent. A page reaching for these gets a message that
    // says which API it wanted, and the name reaches the snapshot.
    // `self` is `window` under another name, and worker-shaped code reaches for
    // it first. It has to be the same object, not a copy, or a page that stores
    // state on one and reads it from the other loses it.
    get self() { return globalThis; },

    // 1 is what a display-less engine honestly has; a declared identity states
    // one, and it must be the number `screen` was built from or the two clash.
    devicePixelRatio: identity.screen ? identity.screen.devicePixelRatio : 1,
    scrollTo(x, y) {
      const top = typeof x === "object" && x !== null ? x.top : y;
      api.setScrollTop(api.root(), Number(top) || 0);
    },
    scroll(x, y) { globalThis.scrollTo(x, y); },
    scrollBy(x, y) {
      const by = typeof x === "object" && x !== null ? x.top : y;
      api.setScrollTop(api.root(), document.documentElement.scrollTop + (Number(by) || 0));
    },
    btoa, atob, escape, unescape,

    // Boa defines neither. `reportError` is how a library hands an error to the
    // page's own handler rather than swallowing it, and this engine's console
    // is exactly where that should land.
    reportError: (error) => api.log("error", `reported: ${render(error)}`),
    // Nothing here is ever collected during a page's life, so a registry that
    // never fires its callback is the honest shape rather than a refusal: a
    // page registering one is not asking for anything it will notice missing.
    FinalizationRegistry: class FinalizationRegistry {
      constructor(callback) { this._callback = callback; }
      register() {}
      unregister() { return false; }
    },

    // The constructors, exposed for `instanceof` — which is how library code
    // asks "is this a node?" before deciding what to do with it. `HTMLElement`
    // is `Element` here because this engine has one element class; the check
    // that matters is the one pages actually write.
    Node, Element, Text, Comment, CharacterData, DocumentFragment, DOMTokenList, Range,
    EventTarget, DOMParser, CSSStyleSheet, ShadowRoot,
    HTMLElement: Element,
    // The per-tag constructors, which pages use two ways: `instanceof HTMLAnchorElement` to ask
    // what they are holding, and `extends HTMLButtonElement` to build on one.
    ...Object.fromEntries(
      [
        "Anchor", "Area", "Audio", "Base", "Body", "BR", "Button", "Canvas", "Data",
        "DataList", "Details", "Dialog", "Div", "DList", "Embed", "FieldSet", "Form",
        "Head", "Heading", "HR", "Html", "IFrame", "Image", "Input", "Label", "Legend",
        "LI", "Link", "Map", "Media", "Menu", "Meta", "Meter", "Mod", "Object", "OList",
        "OptGroup", "Option", "Output", "Paragraph", "Param", "Picture", "Pre",
        "Progress", "Quote", "Script", "Select", "Slot", "Source", "Span", "Style",
        "Table", "TableCaption", "TableCell", "TableCol", "TableRow", "TableSection",
        "Template", "TextArea", "Time", "Title", "Track", "UList", "Unknown", "Video",
      ].map((name) => [
        `HTML${name}Element`,
        globalThis[`HTML${name}Element`] ?? Element,
      ]),
    ),
    SVGElement: Element,
    customElements, NodeFilter, NodeIterator, TreeWalker,

    // Interface objects for the shapes this engine builds without a class.
    ...interfaceObjects(),

    // The three legacy factory constructors, which are real and are the only
    // way a great deal of code creates these elements: `new Image()` predates
    // `createElement` in practice and is still what image preloaders write.
    // These *are* constructible, unlike the brands above, so they are
    // functions that build the element they name.
    Image: makeElementFactory("img", ["width", "height"]),
    Audio: makeElementFactory("audio", ["src"]),
    Option: makeElementFactory("option", ["text", "value", "defaultSelected", "selected"]),

    // Serialising a node to a string. `XMLSerializer` is how a page turns a
    // subtree back into markup without going through `innerHTML` on a parent
    // it may not have, and it is what `DOMParser`'s round trip is usually
    // paired with — we shipped the parser and not the serialiser.
    XMLSerializer: class XMLSerializer {
      serializeToString(node) {
        if (!node) return "";
        if (node.nodeType === 9) return node.documentElement ? node.documentElement.outerHTML : "";
        if (node.outerHTML !== undefined) return node.outerHTML;
        if (node.innerHTML !== undefined) return node.innerHTML;
        return String(node.textContent ?? "");
      }
    },

    // Classes this engine already had and never exposed. Each has a real
    // implementation above; the only thing missing was the name, so
    // `rule instanceof CSSStyleRule` was a ReferenceError over an object that
    // was exactly that.
    CSSRule, CSSStyleRule, CSSGroupingRule,
    CSSStyleDeclaration: StyleDeclaration,
    Response,
    // The event types added beside `InputEvent`.
    FocusEvent, WheelEvent, PointerEvent, CompositionEvent, ErrorEvent,
    PromiseRejectionEvent, ProgressEvent, MessageEvent, CloseEvent, StorageEvent,
    PopStateEvent, HashChangeEvent, PageTransitionEvent, SubmitEvent,
    FormDataEvent, ToggleEvent, CommandEvent, AnimationEvent, TransitionEvent,

    crypto: observed(crypto, "crypto"),
    TextEncoder, TextDecoder, XMLHttpRequest, Blob, File, DOMException,
    getComputedStyle: (element) => {
      // Reads what Stylo resolved. Properties outside the curated set record
      // themselves as unsupported rather than returning a plausible lie: a
      // wrong `display` sends a framework down a branch a real browser never
      // would, and it would never find out.
      if (!element || element._id === undefined) return { getPropertyValue: () => "" };
      const read = (name) => api.computedStyle(element._id, String(name)) || "";
      return new Proxy(
        { getPropertyValue: read },
        {
          get(target, key) {
            if (typeof key !== "string" || key in target) return Reflect.get(target, key);
            return read(camelToDash(key));
          },
          // `"color" in getComputedStyle(el)` asks `has`, not `get`, and without this trap it
          // fell through to the bare backing object and answered **false for every property**.
          has(target, key) {
            if (typeof key !== "string") return Reflect.has(target, key);
            if (key in target) return true;
            return api.isCssProperty(camelToDash(key));
          },
        }
      );
    },
    localStorage: makeStorage(),
    sessionStorage: makeStorage(),
    CustomEvent, UIEvent, MouseEvent, KeyboardEvent, InputEvent,
    DocumentFragment, Headers, Request, AbortController, AbortSignal, FormData,
    MutationObserver,
    IntersectionObserver, ResizeObserver,
  });

  // The display, when the identity declares one: its own tier, loaded here
  // rather than on a property read. `screen` behind an accessor would be the
  // tell this feature exists to avoid — a page reads descriptors first. See
  // `prelude/screen.js`.
  if (identity.screen) __h5iTier("screen");

  // Interface objects are **not enumerable** on the global, and every one of ours was.
  for (const name of Object.getOwnPropertyNames(globalThis)) {
    if (!/^[A-Z]/.test(name)) continue;
    const descriptor = Object.getOwnPropertyDescriptor(globalThis, name);
    if (!descriptor || !descriptor.enumerable || typeof descriptor.value !== "function") {
      continue;
    }
    Object.defineProperty(globalThis, name, {
      value: descriptor.value,
      writable: true,
      enumerable: false,
      configurable: true,
    });
  }

  // The interface-prototype mirror lives in the `conformance` tier
  // (`prelude/conformance.js`), for the reason that file exists: WebIDL puts a
  // member on the interface prototype, this engine's singletons are object
  // literals, and reconciling the two is a shape only a conformance harness
  // inspects. Eagerly parsed it cost 2 KiB — about 90 us of run per page and
  // 490 us of compile on the first — for a property no page reads. The tier is
  // evaluated earlier than this point, so it leaves the work behind as a
  // callback rather than doing it where the globals did not exist yet.
  internals.mirrorSingletons?.();

  // `fetch`, over the host's broker. Every request is policy-checked and
  // receipted before it moves, which is the property this engine exists for.
  function fetch(input, init) {
    const request = input instanceof Request ? input : new Request(input, init);
    const signal = (init && init.signal) || request.signal;
    if (signal && signal.aborted) {
      return Promise.reject(signal.reason ?? abortError());
    }

    let body = request.body ?? "";
    if (body instanceof FormData) body = body.toString();
    else if (body && typeof body !== "string") {
      try { body = JSON.stringify(body); } catch (_) { body = String(body); }
    }

    // Handed to the host and answered later. The whole point of the ticket is
    // that two calls to `fetch` overlap: the old binding did the round trip
    // inline, so a page that fanned out ten requests paid for them in series
    // and every SPA waterfall was ours rather than the site's.
    // The origin story travels with the request. The host decides what may be
    // sent and what may be read from it; this side only reports what the page
    // asked for, because a page that could choose its own answer to those
    // questions would not be subject to a policy at all.
    const headerPairs = [];
    for (const [name, value] of request.headers) headerPairs.push([name, value]);
    const id = api.fetchStart(
      request.url, request.method, body,
      request.mode, request.credentials, headerPairs,
    );
    return new Promise((resolve, reject) => {
      pendingFetches.set(id, { resolve, reject, request, signal });
      // **Abort rejects now, not when the network answers.** The old shape
      // checked `signal.aborted` only at drain time, so an `abort()` against a
      // slow server rejected whenever the server got around to it — and
      // against one that never answers, never. 260 of 467 fetch files timed
      // out on exactly this. The wire request is not cancelled — the thread
      // runs to completion and its receipt stands, because the request *was*
      // made — but the page's promise settles the moment the page said stop,
      // which is the half of abort a page can observe.
      if (signal) {
        signal.addEventListener("abort", () => {
          const waiting = pendingFetches.get(id);
          if (!waiting) return;
          pendingFetches.delete(id);
          waiting.reject(signal.reason ?? abortError());
        });
      }
    });
  }
  globalThis.fetch = fetch;

  const pendingFetches = new Map();

  function responseFrom(res, request) {
    const headers = new Headers();
    for (const [name, value] of res.headers || []) headers.append(name, value);
    // A real `Response`, so `res instanceof Response` answers the way a page
    // expects. It used to be an object literal with the same fields, which
    // reads identically until something asks what it is.
    return new Response(res.text, {
      status: res.status,
      statusText: res.status === 200 ? "OK" : "",
      headers,
      // What a page checks to find out it was handed an opaque response rather
      // than a failed one. Reported rather than left to be inferred from an
      // empty body with status 0, which reads as a network error.
      type: res.opaque ? "opaque" : "basic",
      url: res.url,
      // An opaque response reports no URL, so it cannot report a redirect
      // either: comparing an empty string to the request's URL said every
      // opaque read had been redirected.
      redirected: res.opaque ? false : res.url !== request.url,
    });
  }

  // Driven by the settle loop, so a page's promises resolve as the network
  // answers rather than at some arbitrary later point. Returns how much is
  // still owed, which is what tells `settle` there is real work outstanding.
  globalThis.__h5iDrainFetches = function () {
    for (const [id, res] of api.fetchDrain()) {
      const waiting = pendingFetches.get(id);
      if (!waiting) continue;
      pendingFetches.delete(id);
      if (waiting.signal && waiting.signal.aborted) {
        waiting.reject(waiting.signal.reason ?? abortError());
      } else if (res.error) {
        waiting.reject(new Error(res.error));
      } else {
        waiting.resolve(responseFrom(res, waiting.request));
      }
    }
    return api.fetchPending();
  };

  // Everything still owed an answer when the page ran out of budget. Rejecting
  // is the honest end: a promise left pending forever is a page that looks like
  // it is still working when nothing is.
  globalThis.__h5iAbandonFetches = function (why) {
    for (const [, waiting] of pendingFetches) waiting.reject(new Error(why));
    pendingFetches.clear();
  };

  // In memory and nowhere else. A disposable box has no business writing a
  // page's storage to a filesystem, and "restart the session" is a complete
  // clear — the same rule the cookie jar follows.
  function makeStorage() {
    const map = new Map();
    const api = {
      getItem(k) { const v = map.get(String(k)); return v === undefined ? null : v; },
      setItem(k, v) { map.set(String(k), String(v)); },
      removeItem(k) { map.delete(String(k)); },
      clear() { map.clear(); },
      key(i) { return [...map.keys()][i] ?? null; },
      get length() { return map.size; },
    };

    // `storage.theme` is not a property that might be missing — it *is* the Storage API for a
    // key, and it reads and writes the same map `getItem` does.
    return new Proxy(api, {
      get(target, key) {
        if (typeof key === "symbol" || key in target) return Reflect.get(target, key);
        const value = map.get(String(key));
        return value === undefined ? undefined : value;
      },
      set(target, key, value) {
        if (typeof key === "symbol" || key in target) return Reflect.set(target, key, value);
        map.set(String(key), String(value));
        return true;
      },
      has(target, key) {
        return key in target || map.has(String(key));
      },
      deleteProperty(target, key) {
        if (key in target) return Reflect.deleteProperty(target, key);
        map.delete(String(key));
        return true;
      },
      ownKeys() { return [...map.keys()]; },
      getOwnPropertyDescriptor(target, key) {
        if (key in target) return Reflect.getOwnPropertyDescriptor(target, key);
        if (!map.has(String(key))) return undefined;
        return { value: map.get(String(key)), writable: true, enumerable: true, configurable: true };
      },
    });
  }
})();
