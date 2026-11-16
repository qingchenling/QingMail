//! CSS 令牌解析与允许列表；不保留 URL、变量、导入、字体源及动画。
use cssparser::{
    AtRuleParser, CowRcStr, DeclarationParser, ParseError, Parser, ParserState,
    QualifiedRuleParser, RuleBodyItemParser, RuleBodyParser, StyleSheetParser, ToCss, Token,
};

const PROPERTIES: &[&str] = &[
    "color",
    "background",
    "background-color",
    "font",
    "font-family",
    "font-size",
    "font-style",
    "font-weight",
    "line-height",
    "letter-spacing",
    "text-align",
    "text-decoration",
    "text-indent",
    "white-space",
    "word-break",
    "overflow-wrap",
    "margin",
    "margin-top",
    "margin-right",
    "margin-bottom",
    "margin-left",
    "padding",
    "padding-top",
    "padding-right",
    "padding-bottom",
    "padding-left",
    "border",
    "border-top",
    "border-right",
    "border-bottom",
    "border-left",
    "border-color",
    "border-width",
    "border-style",
    "border-radius",
    "border-collapse",
    "border-spacing",
    "width",
    "min-width",
    "max-width",
    "height",
    "min-height",
    "max-height",
    "display",
    "vertical-align",
    "table-layout",
    "list-style-type",
];

pub(crate) fn declarations(source: &str) -> String {
    let mut parser = Parser::new(source);
    parse_declarations(&mut parser)
}

fn parse_declarations(parser: &mut Parser<'_>) -> String {
    RuleBodyParser::new(parser, &mut Rules)
        .take(256)
        .filter_map(Result::ok)
        .collect::<Vec<_>>()
        .join("")
}

pub(crate) fn stylesheet(source: &str) -> String {
    let mut parser = Parser::new(source);
    StyleSheetParser::new(&mut parser, &mut Rules)
        .take(256)
        .filter_map(Result::ok)
        .collect::<Vec<_>>()
        .join("")
        .replace('<', "\\3c ")
        .replace('>', "\\3e ")
}

struct Rules;

impl<'i> DeclarationParser<'i> for Rules {
    type Declaration = String;
    type Error = ();

    fn parse_value(
        &mut self,
        name: CowRcStr<'i>,
        input: &mut Parser<'i>,
        _: &ParserState,
    ) -> Result<String, ParseError<()>> {
        let name = name.to_ascii_lowercase();
        if !PROPERTIES.contains(&name.as_str()) {
            return Err(ParseError::custom(()));
        }
        let value = value(input, 0)?;
        if value.trim().is_empty() || value.len() > 1024 {
            return Err(ParseError::custom(()));
        }
        Ok(format!("{name}:{value};"))
    }
}

fn value<'i>(input: &mut Parser<'i>, depth: usize) -> Result<String, ParseError<()>> {
    if depth > 4 {
        return Err(ParseError::custom(()));
    }
    let mut output = String::new();
    while !input.is_exhausted() {
        let token = input.next_including_whitespace()?.clone();
        match &token {
            Token::Ident(_)
            | Token::Hash(_)
            | Token::IDHash(_)
            | Token::QuotedString(_)
            | Token::WhiteSpace(_)
            | Token::Comma
            | Token::Delim('/') => {
                output.push_str(&token.to_css_string());
            }
            Token::Number { value, .. }
            | Token::Percentage {
                unit_value: value, ..
            }
            | Token::Dimension { value, .. }
                if value.is_finite() && value.abs() <= 4096.0 =>
            {
                output.push_str(&token.to_css_string());
            }
            Token::Function(name)
                if matches!(
                    name.to_ascii_lowercase().as_str(),
                    "rgb" | "rgba" | "hsl" | "hsla"
                ) =>
            {
                let nested = input.parse_nested_block(|input| value(input, depth + 1))?;
                output.push_str(&format!("{}({nested})", name.to_ascii_lowercase()));
            }
            _ => return Err(ParseError::custom(())),
        }
        if output.len() > 1024 {
            return Err(ParseError::custom(()));
        }
    }
    Ok(output)
}

impl<'i> QualifiedRuleParser<'i> for Rules {
    type Prelude = String;
    type QualifiedRule = String;
    type Error = ();

    fn parse_prelude(&mut self, input: &mut Parser<'i>) -> Result<String, ParseError<()>> {
        let mut selectors = vec![String::new()];
        while !input.is_exhausted() {
            let token = input.next_including_whitespace()?.clone();
            match token {
                Token::Comma => selectors.push(String::new()),
                Token::Ident(_)
                | Token::Hash(_)
                | Token::IDHash(_)
                | Token::WhiteSpace(_)
                | Token::Delim('.')
                | Token::Delim('*')
                | Token::Delim('>') => {
                    selectors
                        .last_mut()
                        .unwrap()
                        .push_str(&token.to_css_string());
                }
                _ => return Err(ParseError::custom(())),
            }
        }
        if selectors.len() > 32
            || selectors
                .iter()
                .any(|selector| selector.trim().is_empty() || selector.len() > 256)
        {
            return Err(ParseError::custom(()));
        }
        Ok(selectors
            .into_iter()
            .map(|selector| format!(".qingmail-body {}", selector.trim()))
            .collect::<Vec<_>>()
            .join(","))
    }

    fn parse_block(
        &mut self,
        prelude: String,
        _: &ParserState,
        input: &mut Parser<'i>,
    ) -> Result<String, ParseError<()>> {
        Ok(format!("{prelude}{{{}}}", parse_declarations(input)))
    }
}

impl<'i> AtRuleParser<'i> for Rules {
    type Prelude = ();
    type AtRule = String;
    type Error = ();
}

impl RuleBodyItemParser<'_, String, ()> for Rules {
    fn parse_declarations(&self) -> bool {
        true
    }
    fn parse_qualified(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_tokens_and_escaped_properties_cannot_bypass_policy() {
        let cleaned = declarations(
            r#"background:url(https://evil.invalid/p);color:rgb(12, 34, 56);background-image:u\72l('//evil.invalid/x');--track:url(x);width:var(--track);behavior:expression(x);font-family:'</style><script>';padding:12px"#,
        );
        assert!(cleaned.contains("color:rgb(12, 34, 56)"));
        assert!(cleaned.contains("padding:12px"));
        for forbidden in ["url(", "expression(", "var(", "--track", "background-image"] {
            assert!(!cleaned.contains(forbidden), "{cleaned}");
        }
        // Inline styles are serialized as attributes by the HTML sanitizer.
        let sheet = stylesheet(
            "@import url(https://evil.invalid);@font-face{src:url(x)}.card, p {color:red;background:image-set(url(x));padding:8px}",
        );
        assert_eq!(
            sheet,
            ".qingmail-body .card,.qingmail-body p{color:red;padding:8px;}"
        );
    }
}
