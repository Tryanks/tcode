use rushdown::{
    ast::{Arena, KindData, NodeKind, NodeRef, NodeType, PrettyPrint},
    parser::{Context, InlineParser, NoParserOptions, Parser, ParserExtension},
    text::{BlockReader, Reader},
};

#[derive(Debug, Clone)]
pub(super) struct MathSpan {
    pub source: String,
    pub display: bool,
    pub selection: super::math_layout::FormulaSelection,
}

impl PartialEq for MathSpan {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source && self.display == other.display
    }
}

impl NodeKind for MathSpan {
    fn typ(&self) -> NodeType {
        NodeType::Inline
    }
    fn kind_name(&self) -> &'static str {
        "Math"
    }
}

impl PrettyPrint for MathSpan {
    fn pretty_print(&self, w: &mut dyn std::fmt::Write, _: &str, _: usize) -> std::fmt::Result {
        w.write_str(&self.source)
    }
}

#[derive(Debug)]
pub(super) struct MathParser;

impl ParserExtension for MathParser {
    fn apply(self, parser: &mut Parser) {
        parser.add_inline_parser(
            || Box::new(MathParser) as Box<dyn InlineParser>,
            NoParserOptions,
            50,
        );
    }
}

impl InlineParser for MathParser {
    fn trigger(&self) -> &[u8] {
        b"$\\"
    }

    fn parse(
        &self,
        arena: &mut Arena,
        _: NodeRef,
        reader: &mut BlockReader,
        _: &mut Context,
    ) -> Option<NodeRef> {
        let (line, _) = reader.peek_line()?;
        let (open, close, display) = if line.starts_with("$$") {
            ("$$", "$$", true)
        } else if line.starts_with('$') && !line[1..].starts_with(char::is_whitespace) {
            ("$", "$", false)
        } else if line.starts_with(r"\[") {
            (r"\[", r"\]", true)
        } else if line.starts_with(r"\(") {
            (r"\(", r"\)", false)
        } else {
            return None;
        };
        let saved = reader.position();
        reader.advance(open.len());
        let mut source = String::new();
        while let Some((line, _)) = reader.peek_line() {
            let mut escaped = false;
            for (ix, ch) in line.char_indices() {
                if !escaped
                    && line[ix..].starts_with(close)
                    && (close != "$"
                        || (ix > 0
                            && !line[..ix].ends_with(char::is_whitespace)
                            && !line[ix + 1..].starts_with(|c: char| c.is_ascii_digit())))
                {
                    source.push_str(&line[..ix]);
                    reader.advance(ix + close.len());
                    return Some(arena.new_node(KindData::Extension(Box::new(MathSpan {
                        source,
                        display,
                        selection: Default::default(),
                    }))));
                }
                escaped = ch == '\\' && !escaped;
            }
            if !display {
                break;
            }
            source.push_str(&line);
            reader.advance_line();
        }
        reader.set_position(saved.0, saved.1);
        None
    }
}
