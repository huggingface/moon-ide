// moon-ide page bridge (ADR 0088). Injected by the preview proxy into
// every HTML document shown in an IDE browser tab. Talks to the IDE
// (the frame's parent) over postMessage so the coder can read and drive
// the page. Plain ES2019, no dependencies; must never contain a closing
// script tag since it's inlined.
(function () {
	'use strict';
	if (window.moonIdeBridge || window.parent === window || window.parent !== window.top) {
		return;
	}
	window.moonIdeBridge = true;

	const MARK = 'moonIdeBridge';
	const MAX_SNAPSHOT_CHARS = 30000;
	const MAX_LINE_CHARS = 200;
	const MAX_CONSOLE = 200;
	const MAX_RESULT_CHARS = 20000;

	function post(msg) {
		msg[MARK] = 1;
		try {
			window.parent.postMessage(msg, '*');
		} catch {
			// Parent went away mid-teardown; nothing to tell.
		}
	}

	// ---- console capture -------------------------------------------------
	let consoleBuf = [];
	function stringify(value) {
		if (typeof value === 'string') {
			return value;
		}
		if (value instanceof Error) {
			return value.stack || String(value);
		}
		try {
			const json = JSON.stringify(value);
			return json === undefined ? String(value) : json;
		} catch {
			return String(value);
		}
	}
	function record(level, args) {
		const text = Array.prototype.map.call(args, stringify).join(' ');
		consoleBuf.push({ level: level, text: text.slice(0, 2000), t: Date.now() });
		if (consoleBuf.length > MAX_CONSOLE) {
			consoleBuf.shift();
		}
	}
	['log', 'info', 'warn', 'error', 'debug'].forEach(function (level) {
		const original = console[level];
		console[level] = function () {
			record(level, arguments);
			return original.apply(console, arguments);
		};
	});
	window.addEventListener('error', function (e) {
		record('error', [e.error || e.message]);
	});
	window.addEventListener('unhandledrejection', function (e) {
		record('error', ['Unhandled rejection:', e.reason]);
	});

	// ---- location tracking ----------------------------------------------
	let lastHref = null;
	function reportLocation(kind) {
		if (kind !== 'hello' && location.href === lastHref) {
			return;
		}
		lastHref = location.href;
		post({ kind: kind, href: location.href, title: document.title });
	}
	['pushState', 'replaceState'].forEach(function (name) {
		const original = history[name];
		history[name] = function () {
			const out = original.apply(history, arguments);
			reportLocation('location');
			return out;
		};
	});
	window.addEventListener('popstate', function () {
		reportLocation('location');
	});
	window.addEventListener('hashchange', function () {
		reportLocation('location');
	});

	// ---- snapshot --------------------------------------------------------
	let refs = new Map();
	let nextRef = 1;
	const INTERACTIVE =
		'a[href],button,input,select,textarea,summary,[role=button],[role=link],[role=checkbox],[role=tab],[role=menuitem],[contenteditable=""],[contenteditable=true],[onclick]';
	const BLOCK = /^(P|LI|TD|TH|DT|DD|LABEL|H[1-6]|PRE|BLOCKQUOTE|FIGCAPTION|CAPTION|LEGEND|OPTION)$/;

	function clip(text, max) {
		text = text.replace(/\s+/g, ' ').trim();
		return text.length > max ? text.slice(0, max) + '…' : text;
	}
	function visible(el) {
		if (el.hidden || el.getAttribute('aria-hidden') === 'true') {
			return false;
		}
		const style = getComputedStyle(el);
		if (style.display === 'none' || style.visibility === 'hidden') {
			return false;
		}
		if (el.tagName === 'INPUT' && el.type === 'hidden') {
			return false;
		}
		return el.getClientRects().length > 0 || style.display === 'contents';
	}
	function refFor(el) {
		for (const entry of refs) {
			if (entry[1] === el) {
				return entry[0];
			}
		}
		const ref = 'e' + nextRef++;
		refs.set(ref, el);
		return ref;
	}
	function label(el) {
		const aria = el.getAttribute('aria-label');
		if (aria) {
			return aria;
		}
		if (el.labels && el.labels.length > 0) {
			return el.labels[0].innerText;
		}
		return el.innerText || el.value || el.getAttribute('title') || el.getAttribute('alt') || '';
	}
	function describe(el) {
		const tag = el.tagName.toLowerCase();
		const role = el.getAttribute('role');
		const parts = ['[' + refFor(el) + ']'];
		if (tag === 'a') {
			parts.push('link "' + clip(label(el), MAX_LINE_CHARS) + '" -> ' + el.getAttribute('href'));
		} else if (tag === 'input') {
			const type = el.type || 'text';
			parts.push('input[' + type + ']');
			if (el.name) {
				parts.push('name=' + el.name);
			}
			const lbl = el.labels && el.labels.length > 0 ? el.labels[0].innerText : el.getAttribute('aria-label');
			if (lbl) {
				parts.push('"' + clip(lbl, 80) + '"');
			}
			if (type === 'checkbox' || type === 'radio') {
				parts.push(el.checked ? 'checked' : 'unchecked');
			} else if (type !== 'password') {
				parts.push('value="' + clip(el.value || '', 80) + '"');
			}
			if (el.placeholder) {
				parts.push('placeholder="' + clip(el.placeholder, 80) + '"');
			}
		} else if (tag === 'select') {
			const selected = el.selectedOptions && el.selectedOptions[0];
			parts.push('select' + (el.name ? ' name=' + el.name : ''));
			parts.push('selected="' + clip(selected ? selected.text : '', 80) + '"');
			parts.push(
				'options=[' +
					Array.prototype.slice
						.call(el.options, 0, 20)
						.map(function (o) {
							return JSON.stringify(clip(o.text, 40));
						})
						.join(', ') +
					']',
			);
		} else if (tag === 'textarea') {
			parts.push('textarea' + (el.name ? ' name=' + el.name : ''));
			parts.push('value="' + clip(el.value || '', 120) + '"');
		} else {
			parts.push((role || tag) + ' "' + clip(label(el), MAX_LINE_CHARS) + '"');
		}
		if (el.disabled) {
			parts.push('disabled');
		}
		return parts.join(' ');
	}
	function snapshot() {
		refs = new Map();
		nextRef = 1;
		const lines = [];
		let size = 0;
		let truncated = false;
		function push(depth, line) {
			if (truncated) {
				return;
			}
			const text = '  '.repeat(Math.min(depth, 8)) + line;
			size += text.length + 1;
			if (size > MAX_SNAPSHOT_CHARS) {
				truncated = true;
				return;
			}
			lines.push(text);
		}
		function walk(el, depth) {
			if (truncated || !visible(el)) {
				return;
			}
			if (/^(SCRIPT|STYLE|NOSCRIPT|TEMPLATE|SVG)$/.test(el.tagName)) {
				return;
			}
			if (el.matches(INTERACTIVE)) {
				push(depth, describe(el));
				return;
			}
			if (/^H[1-6]$/.test(el.tagName)) {
				push(depth, '#'.repeat(Number(el.tagName[1])) + ' ' + clip(el.innerText, MAX_LINE_CHARS));
				return;
			}
			let childDepth = depth;
			if (/^(NAV|MAIN|HEADER|FOOTER|ASIDE|FORM|DIALOG|TABLE|UL|OL|SECTION)$/.test(el.tagName)) {
				const name = el.getAttribute('aria-label') || el.id || '';
				push(depth, el.tagName.toLowerCase() + (name ? ' "' + clip(name, 60) + '"' : '') + ':');
				childDepth = depth + 1;
			}
			let own = '';
			for (let node = el.firstChild; node; node = node.nextSibling) {
				if (node.nodeType === 3) {
					own += node.textContent;
				} else if (node.nodeType === 1) {
					if (own.trim()) {
						push(childDepth, clip(own, MAX_LINE_CHARS));
						own = '';
					}
					walk(node, childDepth);
				}
			}
			if (own.trim()) {
				push(childDepth, clip(own, MAX_LINE_CHARS));
			} else if (BLOCK.test(el.tagName) && el.childElementCount === 0 && el.innerText) {
				push(childDepth, clip(el.innerText, MAX_LINE_CHARS));
			}
		}
		if (document.body) {
			walk(document.body, 0);
		}
		return {
			href: location.href,
			title: document.title,
			snapshot: lines.join('\n'),
			truncated: truncated,
		};
	}

	// ---- actions ---------------------------------------------------------
	function target(args) {
		let el = null;
		if (args.ref) {
			el = refs.get(args.ref) || null;
			if (el && !el.isConnected) {
				throw new Error('ref ' + args.ref + ' is gone from the page; take a new snapshot');
			}
			if (!el) {
				throw new Error('unknown ref ' + args.ref + '; take a new snapshot');
			}
		} else if (args.selector) {
			el = document.querySelector(args.selector);
			if (!el) {
				throw new Error('no element matches ' + args.selector);
			}
		} else {
			throw new Error('pass `ref` (from a snapshot) or `selector`');
		}
		el.scrollIntoView({ block: 'center', inline: 'center' });
		return el;
	}
	function setValue(el, value) {
		const proto =
			el.tagName === 'TEXTAREA'
				? HTMLTextAreaElement.prototype
				: el.tagName === 'SELECT'
					? HTMLSelectElement.prototype
					: HTMLInputElement.prototype;
		// The prototype setter, not `el.value =`, so framework-tracked
		// inputs (React) notice the change.
		Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, value);
		el.dispatchEvent(new Event('input', { bubbles: true }));
		el.dispatchEvent(new Event('change', { bubbles: true }));
	}
	function key(el, name) {
		const init = { key: name, code: name, bubbles: true, cancelable: true };
		el.dispatchEvent(new KeyboardEvent('keydown', init));
		el.dispatchEvent(new KeyboardEvent('keypress', init));
		el.dispatchEvent(new KeyboardEvent('keyup', init));
	}
	function page() {
		return { href: location.href, title: document.title };
	}
	function sleep(ms) {
		return new Promise(function (resolve) {
			setTimeout(resolve, ms);
		});
	}
	function serialize(value) {
		if (value === undefined) {
			return 'undefined';
		}
		let text;
		if (value instanceof Element) {
			text = value.outerHTML;
		} else {
			try {
				text = JSON.stringify(value, null, 2);
			} catch {
				text = String(value);
			}
			if (text === undefined) {
				text = String(value);
			}
		}
		return text.length > MAX_RESULT_CHARS ? text.slice(0, MAX_RESULT_CHARS) + '… [truncated]' : text;
	}

	const ops = {
		snapshot: function () {
			return snapshot();
		},
		click: function (args) {
			const el = target(args);
			if (el.focus) {
				el.focus();
			}
			el.click();
			return page();
		},
		type: function (args) {
			const el = target(args);
			const text = String(args.text ?? '');
			el.focus();
			if (el.isContentEditable) {
				el.textContent = args.clear === false ? el.textContent + text : text;
				el.dispatchEvent(new Event('input', { bubbles: true }));
			} else {
				setValue(el, args.clear === false ? (el.value || '') + text : text);
			}
			if (args.submit) {
				if (el.form && el.form.requestSubmit) {
					el.form.requestSubmit();
				} else {
					key(el, 'Enter');
				}
			}
			return page();
		},
		select: function (args) {
			const el = target(args);
			if (el.tagName !== 'SELECT') {
				throw new Error('element is not a <select>');
			}
			const wanted = String(args.value);
			const option = Array.prototype.find.call(el.options, function (o) {
				return o.value === wanted || o.text.trim() === wanted;
			});
			if (!option) {
				throw new Error('no option matching ' + JSON.stringify(wanted));
			}
			setValue(el, option.value);
			return page();
		},
		press: function (args) {
			key(document.activeElement || document.body, String(args.key));
			return page();
		},
		eval: function (args) {
			// Running agent-supplied JS is this action's whole point;
			// indirect so it evaluates in global scope like devtools.
			// oxlint-disable-next-line no-eval
			return Promise.resolve((0, eval)(String(args.expression))).then(function (value) {
				return { value: serialize(value) };
			});
		},
		console: function (args) {
			const entries = consoleBuf.slice();
			if (args.clear) {
				consoleBuf = [];
			}
			return { entries: entries };
		},
		wait_for: function (args) {
			const deadline = Date.now() + Math.min(Number(args.timeout_ms) || 5000, 15000);
			function found() {
				if (args.selector) {
					return document.querySelector(args.selector) !== null;
				}
				if (args.text) {
					return document.body !== null && document.body.innerText.indexOf(args.text) >= 0;
				}
				throw new Error('pass `selector` or `text`');
			}
			function loop() {
				if (found()) {
					return page();
				}
				if (Date.now() > deadline) {
					throw new Error('timed out waiting');
				}
				return sleep(100).then(loop);
			}
			return loop();
		},
	};

	window.addEventListener('message', function (event) {
		const msg = event.data;
		if (event.source !== window.parent || !msg || msg[MARK] !== 1 || msg.kind !== 'request') {
			return;
		}
		const op = ops[msg.op];
		Promise.resolve()
			.then(function () {
				if (!op) {
					throw new Error('unknown page action ' + msg.op);
				}
				return op(msg.args || {});
			})
			.then(
				function (value) {
					post({ kind: 'result', id: msg.id, ok: true, value: value });
				},
				function (err) {
					post({ kind: 'result', id: msg.id, ok: false, error: String((err && err.message) || err) });
				},
			);
	});

	reportLocation('hello');
})();
