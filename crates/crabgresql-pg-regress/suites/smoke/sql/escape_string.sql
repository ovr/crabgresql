--
-- Escape string constants: E'...', U&'...', N'...'
-- PostgreSQL decodes E'...' escapes into *bytes* and validates the result as
-- UTF-8 only once the whole literal is assembled, so several escapes can spell
-- one character while a lone high byte is an encoding error. U&'...' escapes
-- name code points directly instead.
--
-- the C-style escapes. Compared rather than printed: the harness aligns
-- columns by character count, so a tab or a newline in the output would be
-- measuring its renderer instead of the decoding.
SELECT E'a\tb' = 'a' || chr(9) || 'b' AS tab,
       E'a\nb' = 'a' || chr(10) || 'b' AS newline,
       E'a\rb' = 'a' || chr(13) || 'b' AS carriage_return,
       E'a\bb' = 'a' || chr(8) || 'b' AS backspace,
       E'a\fb' = 'a' || chr(12) || 'b' AS form_feed;
-- a backslash before anything without a meaning simply drops out
SELECT E'\z' AS bare_z, E'\'' AS quote, E'\\' AS backslash;
-- a doubled quote still closes nothing, exactly as in a plain literal
SELECT E'it''s' AS doubled_quote;
-- quote_literal round-trips a backslash back into an escape string constant
SELECT quote_literal(e'\\') AS quoted_backslash;
-- \xNN is a byte, so a two-byte UTF-8 sequence takes two escapes
SELECT E'\xc3\xa9' AS e_acute, octet_length(E'\xc3\xa9') AS octets;
-- three bytes for a CJK character, and the same character via \u
SELECT E'\xe4\xb8\xad' = E'中' AS zhong, octet_length(E'\xe4\xb8\xad') AS zhong_octets;
-- octal escapes address bytes just as hex ones do
SELECT E'\303\251' AS e_acute_octal, E'\303\251' = E'\xc3\xa9' AS same_as_hex;
-- \x with no hex digit at all leaves a literal x
SELECT E'\x' AS bare_x, E'\xP' AS x_then_p;
-- octal overflows by truncation rather than erroring: \450 is 0o450 & 0xFF
SELECT E'\450' AS octal_wrapped, E'\123' AS octal_S;
-- \uXXXX and \UXXXXXXXX name code points; a surrogate pair combines into one
SELECT E'A' AS letter_a,
       E'\U0001F604' = E'😄' AS surrogate_pair_combines,
       length(E'\U0001F604') AS emoji_len;
-- an escape string is still just text, so it takes a type from its context
SELECT E'abc'::name AS as_name, length(E'a\tb') AS len;
-- U&'...' escapes are code points, with \+XXXXXX for the six-digit form
SELECT U&'d\0061t\+000061' AS unicode_data;
-- and pair surrogates just as E'...' does
SELECT U&'\D83D\DE04' = U&'\+01F604' AS u_surrogate_pair;
-- N'...' is a plain string literal on a UTF-8 server, doubled quotes and all
SELECT N'abc' AS national, N'it''s' AS national_quote;
-- every spelling is the same kind of constant, so all are accepted wherever
-- one is: as an ESCAPE character, and as a typed or interval literal
SELECT 'a_b' LIKE 'a\_b' ESCAPE E'\\' AS like_escape,
       'a_b' SIMILAR TO 'a\_b' ESCAPE E'\\' AS similar_escape;
SELECT DATE E'2024-01-01' AS typed_literal, INTERVAL E'1 day' AS interval_literal;
-- a lone continuation byte is not valid UTF-8. The reported bytes are the
-- length the lead byte promises, clipped to what is actually present
SELECT E'\x80';
-- neither is a truncated two-byte sequence
SELECT E'\xc3';
SELECT E'\xe4\xb8';
-- \xCA promises two bytes, so the offending trailing byte is named too
SELECT E'\xCAD';
-- NUL is valid UTF-8 but PG rejects it however it is spelled
SELECT E'\0';
SELECT E'\000';
-- \400 truncates to 0x00, so it is the same rejection
SELECT E'\400';
-- a \u escape that is not four hex digits is a *malformed* escape, and is the
-- only one of these conditions that carries a hint
SELECT E'\uZZZZ';
-- whereas a well-formed escape naming something that is not a code point is an
-- escape *value* error: past the last code point, or zero
SELECT E'\U00110000';
SELECT U&'\+110000';
-- a code point escape naming zero is rejected before any byte is produced, so
-- it is this error rather than the encoding one \0 above gives
SELECT U&'\0000';
-- a high surrogate that nothing completes is reported on what followed it
SELECT E'\ud83d';
SELECT E'\ud83dxyz';
-- an unpaired low surrogate is reported on the escape itself
SELECT E'\udc36';
-- the same rules hold for U&'...', which does not quote the offending escape
SELECT U&'\D83D';
SELECT U&'\DE04';
-- a U&'...' escape has its own hint, naming the \XXXX forms
SELECT U&'\ZZZZ';
