import { describe, expect, it } from 'vitest';

import {
	collectMentionedUserIds,
	decodeEntities,
	parseSlackMrkdwn,
	slackPlainText,
	type BlockNode,
} from './slackMrkdwn';

// Tiny helpers that pull common shapes out of the parser so the
// assertions below read closer to natural language. Each `single*`
// asserts there's exactly one block of the expected type and returns
// its inline children (or value).
function singleText(blocks: BlockNode[]) {
	expect(blocks).toHaveLength(1);
	const [block] = blocks;
	expect(block?.type).toBe('text');
	if (block?.type !== 'text') {
		throw new Error('unreachable');
	}
	return block.children;
}

function singleQuote(blocks: BlockNode[]) {
	expect(blocks).toHaveLength(1);
	const [block] = blocks;
	expect(block?.type).toBe('quote');
	if (block?.type !== 'quote') {
		throw new Error('unreachable');
	}
	return block.children;
}

describe('parseSlackMrkdwn — plain text', () => {
	it('emits a single text block with one text node', () => {
		const blocks = parseSlackMrkdwn('hello world');
		const inline = singleText(blocks);
		expect(inline).toEqual([{ type: 'text', value: 'hello world' }]);
	});

	it('returns an empty list for empty input', () => {
		expect(parseSlackMrkdwn('')).toEqual([]);
	});

	it('preserves inline newlines inside a single block', () => {
		const blocks = parseSlackMrkdwn('line one\nline two');
		const inline = singleText(blocks);
		expect(inline).toEqual([{ type: 'text', value: 'line one\nline two' }]);
	});
});

describe('parseSlackMrkdwn — formatting', () => {
	it('parses *bold*', () => {
		const inline = singleText(parseSlackMrkdwn('hello *world*'));
		expect(inline).toEqual([
			{ type: 'text', value: 'hello ' },
			{ type: 'bold', children: [{ type: 'text', value: 'world' }] },
		]);
	});

	it('parses _italic_', () => {
		const inline = singleText(parseSlackMrkdwn('hello _world_'));
		expect(inline[1]).toEqual({ type: 'italic', children: [{ type: 'text', value: 'world' }] });
	});

	it('parses ~strike~', () => {
		const inline = singleText(parseSlackMrkdwn('hello ~world~'));
		expect(inline[1]).toEqual({ type: 'strike', children: [{ type: 'text', value: 'world' }] });
	});

	it('does not treat *foo*bar*baz* as bold (word-boundary closer)', () => {
		// The first `*` opens, but the closer must be followed by a
		// non-word char or end. `*bar` fails that, so the whole run
		// stays as text.
		const inline = singleText(parseSlackMrkdwn('foo*bar*baz'));
		expect(inline).toEqual([{ type: 'text', value: 'foo*bar*baz' }]);
	});

	it('does not treat letter-flanked _underscore_ as italic', () => {
		const inline = singleText(parseSlackMrkdwn('snake_case_word'));
		expect(inline).toEqual([{ type: 'text', value: 'snake_case_word' }]);
	});

	it('refuses same-marker nesting (Slack rule)', () => {
		// Inner `*` cannot reopen bold inside bold; the inner pair
		// becomes literal text.
		const inline = singleText(parseSlackMrkdwn('*outer *inner* end*'));
		const bold = inline[0];
		expect(bold).toMatchObject({ type: 'bold' });
		if (bold?.type !== 'bold') {
			throw new Error('unreachable');
		}
		// The inner `*…*` should be flat text, not a nested bold node.
		expect(bold.children.every((c) => c.type === 'text')).toBe(true);
	});

	it('allows different markers to nest', () => {
		const inline = singleText(parseSlackMrkdwn('*bold _and italic_*'));
		expect(inline).toEqual([
			{
				type: 'bold',
				children: [
					{ type: 'text', value: 'bold ' },
					{ type: 'italic', children: [{ type: 'text', value: 'and italic' }] },
				],
			},
		]);
	});

	it('treats a stray asterisk as text', () => {
		const inline = singleText(parseSlackMrkdwn('5 * 4 = 20'));
		expect(inline).toEqual([{ type: 'text', value: '5 * 4 = 20' }]);
	});
});

describe('parseSlackMrkdwn — code', () => {
	it('parses inline `code`', () => {
		const inline = singleText(parseSlackMrkdwn('try `npm test`'));
		expect(inline).toEqual([
			{ type: 'text', value: 'try ' },
			{ type: 'code', value: 'npm test' },
		]);
	});

	it('does not apply formatting inside inline code', () => {
		const inline = singleText(parseSlackMrkdwn('see `*not* bold` ok'));
		expect(inline[1]).toEqual({ type: 'code', value: '*not* bold' });
	});

	it('wraps inline code in surrounding *bold* markers', () => {
		// Slack renders `*\`code\`*` as bold + monospace; the
		// asterisks must not strand as literal text.
		const inline = singleText(parseSlackMrkdwn('see *`npm test`* run'));
		expect(inline).toEqual([
			{ type: 'text', value: 'see ' },
			{ type: 'bold', children: [{ type: 'code', value: 'npm test' }] },
			{ type: 'text', value: ' run' },
		]);
	});

	it('wraps mixed text and inline code in surrounding bold', () => {
		const inline = singleText(parseSlackMrkdwn('a *foo `bar` baz* c'));
		const bold = inline[1];
		expect(bold).toMatchObject({ type: 'bold' });
		if (bold?.type !== 'bold') {
			throw new Error('unreachable');
		}
		expect(bold.children).toEqual([
			{ type: 'text', value: 'foo ' },
			{ type: 'code', value: 'bar' },
			{ type: 'text', value: ' baz' },
		]);
	});

	it('wraps angle tokens in surrounding bold', () => {
		// Same opaque-atom story for `<@U…>` mentions: `*<@U1>*`
		// renders as bold(@user), not literal stars around the
		// mention.
		const inline = singleText(parseSlackMrkdwn('hi *<@U1>*!'));
		const bold = inline[1];
		expect(bold).toMatchObject({ type: 'bold' });
		if (bold?.type !== 'bold') {
			throw new Error('unreachable');
		}
		expect(bold.children).toEqual([{ type: 'userMention', userId: 'U1', label: null }]);
	});

	it('handles italic around inline code', () => {
		const inline = singleText(parseSlackMrkdwn('_`code`_'));
		expect(inline[0]).toEqual({
			type: 'italic',
			children: [{ type: 'code', value: 'code' }],
		});
	});

	it('treats `` (empty inline) as literal', () => {
		const inline = singleText(parseSlackMrkdwn('foo `` bar'));
		// Renderer would print "foo `` bar".
		expect(inline.some((n) => n.type === 'text' && n.value.includes('``'))).toBe(true);
	});

	it('parses a fenced code block', () => {
		const blocks = parseSlackMrkdwn('before\n```\nlet x = 1;\n```\nafter');
		expect(blocks).toHaveLength(3);
		expect(blocks[1]).toEqual({ type: 'codeblock', value: 'let x = 1;\n' });
	});

	it('keeps an unclosed fence as literal text', () => {
		const blocks = parseSlackMrkdwn('hello ```\nstill text');
		// Whole tail stays in text blocks (no codeblock emitted).
		expect(blocks.every((b) => b.type !== 'codeblock')).toBe(true);
	});
});

describe('parseSlackMrkdwn — quotes', () => {
	it('parses a single quoted line', () => {
		const blocks = parseSlackMrkdwn('> a quoted line');
		const inline = singleQuote(blocks);
		expect(inline).toEqual([{ type: 'text', value: 'a quoted line' }]);
	});

	it('groups consecutive quote lines into one block', () => {
		const blocks = parseSlackMrkdwn('> first\n> second');
		const inline = singleQuote(blocks);
		expect(inline).toEqual([{ type: 'text', value: 'first\nsecond' }]);
	});

	it('separates quote and non-quote runs', () => {
		const blocks = parseSlackMrkdwn('intro\n> quoted\noutro');
		expect(blocks.map((b) => b.type)).toEqual(['text', 'quote', 'text']);
	});

	it('respects formatting inside a quote', () => {
		const inline = singleQuote(parseSlackMrkdwn('> hello *bold*'));
		expect(inline).toEqual([
			{ type: 'text', value: 'hello ' },
			{ type: 'bold', children: [{ type: 'text', value: 'bold' }] },
		]);
	});
});

describe('parseSlackMrkdwn — angle tokens', () => {
	it('parses <@U123>', () => {
		const inline = singleText(parseSlackMrkdwn('hi <@U123>'));
		expect(inline[1]).toEqual({ type: 'userMention', userId: 'U123', label: null });
	});

	it('parses <@U123|alice> and strips a leading @ from the label', () => {
		const inline = singleText(parseSlackMrkdwn('<@U123|@alice> hi'));
		expect(inline[0]).toEqual({ type: 'userMention', userId: 'U123', label: 'alice' });
	});

	it('parses <#C123|general>', () => {
		const inline = singleText(parseSlackMrkdwn('see <#C123|general>'));
		expect(inline[1]).toEqual({ type: 'channelMention', channelId: 'C123', label: 'general' });
	});

	it('parses broadcast <!here>', () => {
		const inline = singleText(parseSlackMrkdwn('<!here> attention'));
		expect(inline[0]).toEqual({ type: 'broadcast', kind: 'here', label: null });
	});

	it('parses broadcast with explicit label <!channel|@channel>', () => {
		const inline = singleText(parseSlackMrkdwn('<!channel|@channel>'));
		expect(inline[0]).toEqual({ type: 'broadcast', kind: 'channel', label: '@channel' });
	});

	it('parses <!subteam^S123|@team>', () => {
		const inline = singleText(parseSlackMrkdwn('<!subteam^S123|@team> hi'));
		expect(inline[0]).toEqual({ type: 'usergroup', id: 'S123', label: '@team' });
	});

	it('parses <!date^…> using the fallback', () => {
		const inline = singleText(parseSlackMrkdwn('<!date^1234^{date_pretty}|2026-04-26>'));
		expect(inline[0]).toEqual({ type: 'date', fallback: '2026-04-26' });
	});

	it('parses a bare https link', () => {
		const inline = singleText(parseSlackMrkdwn('go to <https://example.com>'));
		expect(inline[1]).toEqual({ type: 'link', url: 'https://example.com', label: null });
	});

	it('parses a labelled https link', () => {
		const inline = singleText(parseSlackMrkdwn('go <https://example.com|here>'));
		expect(inline[1]).toEqual({ type: 'link', url: 'https://example.com', label: 'here' });
	});

	it('drops <javascript:...> as unknown angle token (kept as literal)', () => {
		// Tokenizer rejects unknown schemes; the chunk becomes literal
		// text so the renderer can't emit it as a clickable link.
		const inline = singleText(parseSlackMrkdwn('click <javascript:alert(1)>'));
		expect(inline.some((n) => n.type === 'link')).toBe(false);
	});

	it('keeps an unclosed <... as literal text', () => {
		const inline = singleText(parseSlackMrkdwn('truncated <https://example.com'));
		// No `>` ⇒ no token; the literal `<` survives.
		expect(inline).toEqual([{ type: 'text', value: 'truncated <https://example.com' }]);
	});
});

describe('decodeEntities', () => {
	it('decodes amp / lt / gt', () => {
		expect(decodeEntities('a &amp; b &lt; c &gt; d')).toBe('a & b < c > d');
	});

	it('decodes decimal numeric entities', () => {
		expect(decodeEntities('snowman: &#9731;')).toBe('snowman: ☃');
	});

	it('decodes hex numeric entities', () => {
		expect(decodeEntities('snowman: &#x2603;')).toBe('snowman: ☃');
	});

	it('leaves unknown entities alone', () => {
		expect(decodeEntities('not &nbsp; here')).toBe('not &nbsp; here');
	});

	it('decodes entities embedded in inline text', () => {
		const inline = singleText(parseSlackMrkdwn('Tom &amp; Jerry'));
		expect(inline).toEqual([{ type: 'text', value: 'Tom & Jerry' }]);
	});
});

describe('collectMentionedUserIds', () => {
	it('returns an empty list for plain text', () => {
		expect(collectMentionedUserIds(parseSlackMrkdwn('hello world'))).toEqual([]);
	});

	it('returns one ID per mention', () => {
		const blocks = parseSlackMrkdwn('hi <@U1> and <@U2>');
		expect(collectMentionedUserIds(blocks)).toEqual(['U1', 'U2']);
	});

	it('deduplicates repeated mentions', () => {
		const blocks = parseSlackMrkdwn('<@U1> hi <@U1> again');
		expect(collectMentionedUserIds(blocks)).toEqual(['U1']);
	});

	it('walks into formatting nodes', () => {
		const blocks = parseSlackMrkdwn('*bold <@U1>* and _italic <@U2>_');
		expect(collectMentionedUserIds(blocks)).toEqual(['U1', 'U2']);
	});

	it('ignores channel and broadcast tokens', () => {
		const blocks = parseSlackMrkdwn('<#C1|gen> and <!here> and <@U1>');
		expect(collectMentionedUserIds(blocks)).toEqual(['U1']);
	});

	it('does not descend into code blocks', () => {
		const blocks = parseSlackMrkdwn('```<@U1>```');
		expect(collectMentionedUserIds(blocks)).toEqual([]);
	});
});

describe('slackPlainText', () => {
	it('flattens formatting to text', () => {
		expect(slackPlainText('hello *bold* and _italic_')).toBe('hello bold and italic');
	});

	it('renders link labels (or URLs) without angle brackets', () => {
		expect(slackPlainText('go <https://example.com|here>')).toBe('go here');
		expect(slackPlainText('see <https://example.com>')).toBe('see https://example.com');
	});

	it('renders mentions with their embedded label first', () => {
		expect(slackPlainText('hi <@U1|alice>')).toBe('hi @alice');
	});

	it('falls back to the user ID without a resolver', () => {
		expect(slackPlainText('hi <@U1>')).toBe('hi @U1');
	});

	it('uses the resolveUserId hook when provided', () => {
		const out = slackPlainText('hi <@U1>', { resolveUserId: (id) => (id === 'U1' ? 'alice' : null) });
		expect(out).toBe('hi @alice');
	});

	it('falls back to the user ID when the resolver returns null', () => {
		const out = slackPlainText('hi <@U2>', { resolveUserId: () => null });
		expect(out).toBe('hi @U2');
	});

	it('strips a dangling <token… and ellipses (preview truncation)', () => {
		// Server cut the preview mid-link; we drop everything from the
		// unclosed `<` onward and append an ellipsis.
		expect(slackPlainText('hello <https://example.com/very-long-pa')).toBe('hello…');
	});

	it('keeps a balanced token even when followed by trailing text', () => {
		expect(slackPlainText('<@U1|alice> said hi')).toBe('@alice said hi');
	});

	it('renders broadcasts as @kind when no label is present', () => {
		expect(slackPlainText('<!here> hello')).toBe('@here hello');
	});

	it('keeps codeblock contents verbatim', () => {
		// The block boundaries each contribute one newline (parser
		// preserves the surrounding text blocks intact). Previews
		// rarely hit this path — server truncation usually cuts before
		// a fence — but lock the behaviour down anyway.
		expect(slackPlainText('before\n```\ncode here\n```\nafter')).toBe('before\n\ncode here\n\n\nafter');
	});

	it('decodes entities through the flatten path', () => {
		expect(slackPlainText('Tom &amp; Jerry')).toBe('Tom & Jerry');
	});
});

describe('parseSlackMrkdwn — emoji shortcodes', () => {
	it('replaces a known shortcode with the matching glyph', () => {
		const inline = singleText(parseSlackMrkdwn('hi :wave:'));
		expect(inline).toEqual([{ type: 'text', value: 'hi 👋' }]);
	});

	it('replaces a Slack-only alias (`:robot_face:` → 🤖)', () => {
		const inline = singleText(parseSlackMrkdwn('beep :robot_face: boop'));
		expect(inline).toEqual([{ type: 'text', value: 'beep 🤖 boop' }]);
	});

	it('replaces multiple shortcodes in one segment', () => {
		const inline = singleText(parseSlackMrkdwn('shipping :rocket: with :tada:'));
		expect(inline).toEqual([{ type: 'text', value: 'shipping 🚀 with 🎉' }]);
	});

	it('passes unknown shortcodes through unchanged', () => {
		const inline = singleText(parseSlackMrkdwn('custom :totally_not_a_real_emoji: here'));
		expect(inline).toEqual([{ type: 'text', value: 'custom :totally_not_a_real_emoji: here' }]);
	});

	it('substitutes inside formatted runs', () => {
		const inline = singleText(parseSlackMrkdwn('*pumped :tada:*'));
		expect(inline).toEqual([{ type: 'bold', children: [{ type: 'text', value: 'pumped 🎉' }] }]);
	});

	it('skips inline code spans', () => {
		const inline = singleText(parseSlackMrkdwn('use `os.environ[":wave:"]` literally'));
		expect(inline).toEqual([
			{ type: 'text', value: 'use ' },
			{ type: 'code', value: 'os.environ[":wave:"]' },
			{ type: 'text', value: ' literally' },
		]);
	});

	it('skips fenced code blocks', () => {
		const blocks = parseSlackMrkdwn('```\n:wave: stays as text\n```');
		expect(blocks).toEqual([{ type: 'codeblock', value: ':wave: stays as text\n' }]);
	});

	it('substitutes inside link labels but not URLs', () => {
		const inline = singleText(parseSlackMrkdwn('go <https://example.com/:wave:|wave :wave:>'));
		expect(inline).toEqual([
			{ type: 'text', value: 'go ' },
			{ type: 'link', url: 'https://example.com/:wave:', label: 'wave 👋' },
		]);
	});

	it('substitutes inside mention labels', () => {
		const inline = singleText(parseSlackMrkdwn('hi <@U1|alice :wave:>'));
		expect(inline).toEqual([
			{ type: 'text', value: 'hi ' },
			{ type: 'userMention', userId: 'U1', label: 'alice 👋' },
		]);
	});

	it('substitutes inside quote blocks', () => {
		const inline = singleQuote(parseSlackMrkdwn('> shipped :rocket:'));
		expect(inline).toEqual([{ type: 'text', value: 'shipped 🚀' }]);
	});

	it('flows through slackPlainText with shortcodes resolved', () => {
		expect(slackPlainText('shipped :rocket: with :tada:')).toBe('shipped 🚀 with 🎉');
	});

	it('leaves a stray colon alone', () => {
		const inline = singleText(parseSlackMrkdwn('time: 12:34'));
		expect(inline).toEqual([{ type: 'text', value: 'time: 12:34' }]);
	});
});
