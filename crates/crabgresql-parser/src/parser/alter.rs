// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! SQL Parser for ALTER

#[cfg(not(feature = "std"))]
use alloc::{string::ToString, vec};

use super::{Parser, ParserError};
use crate::{
    ast::{
        AlterPolicy, AlterPolicyOperation, AlterRoleOperation, Expr, Password, ResetConfig,
        RoleOption, SetConfigValue, Statement,
    },
    dialect::PostgreSqlDialect,
    keywords::Keyword,
    tokenizer::Token,
};

impl Parser<'_> {
    /// Parse `ALTER ROLE` statement
    pub fn parse_alter_role(&mut self) -> Result<Statement, ParserError> {
        if dialect_of!(self is PostgreSqlDialect) {
            return self.parse_pg_alter_role();
        }

        Err(ParserError::ParserError(
            "ALTER ROLE is only support for PostgreSqlDialect".into(),
        ))
    }

    /// Parse ALTER POLICY statement
    /// ```sql
    /// ALTER POLICY policy_name ON table_name [ RENAME TO new_name ]
    /// or
    /// ALTER POLICY policy_name ON table_name
    /// [ TO { role_name | PUBLIC | CURRENT_ROLE | CURRENT_USER | SESSION_USER } [, ...] ]
    /// [ USING ( using_expression ) ]
    /// [ WITH CHECK ( check_expression ) ]
    /// ```
    ///
    /// [PostgreSQL](https://www.postgresql.org/docs/current/sql-alterpolicy.html)
    pub fn parse_alter_policy(&mut self) -> Result<AlterPolicy, ParserError> {
        let name = self.parse_identifier()?;
        self.expect_keyword_is(Keyword::ON)?;
        let table_name = self.parse_object_name(false)?;

        if self.parse_keyword(Keyword::RENAME) {
            self.expect_keyword_is(Keyword::TO)?;
            let new_name = self.parse_identifier()?;
            Ok(AlterPolicy {
                name,
                table_name,
                operation: AlterPolicyOperation::Rename { new_name },
            })
        } else {
            let to = if self.parse_keyword(Keyword::TO) {
                Some(self.parse_comma_separated(|p| p.parse_owner())?)
            } else {
                None
            };

            let using = if self.parse_keyword(Keyword::USING) {
                self.expect_token(&Token::LParen)?;
                let expr = self.parse_expr()?;
                self.expect_token(&Token::RParen)?;
                Some(expr)
            } else {
                None
            };

            let with_check = if self.parse_keywords(&[Keyword::WITH, Keyword::CHECK]) {
                self.expect_token(&Token::LParen)?;
                let expr = self.parse_expr()?;
                self.expect_token(&Token::RParen)?;
                Some(expr)
            } else {
                None
            };
            Ok(AlterPolicy {
                name,
                table_name,
                operation: AlterPolicyOperation::Apply {
                    to,
                    using,
                    with_check,
                },
            })
        }
    }

    fn parse_pg_alter_role(&mut self) -> Result<Statement, ParserError> {
        let role_name = self.parse_identifier()?;

        // [ IN DATABASE _`database_name`_ ]
        let in_database = if self.parse_keywords(&[Keyword::IN, Keyword::DATABASE]) {
            self.parse_object_name(false).ok()
        } else {
            None
        };

        let operation = if self.parse_keyword(Keyword::RENAME) {
            if self.parse_keyword(Keyword::TO) {
                let role_name = self.parse_identifier()?;
                AlterRoleOperation::RenameRole { role_name }
            } else {
                return self.expected_ref("TO after RENAME", self.peek_token_ref());
            }
        } else if self.parse_keyword(Keyword::SET) {
            let config_name = self.parse_object_name(false)?;
            if self.parse_keywords(&[Keyword::FROM, Keyword::CURRENT]) {
                AlterRoleOperation::Set {
                    config_name,
                    config_value: SetConfigValue::FromCurrent,
                    in_database,
                }
            // { TO | = } { value | DEFAULT }
            } else if self.consume_token(&Token::Eq) || self.parse_keyword(Keyword::TO) {
                if self.parse_keyword(Keyword::DEFAULT) {
                    AlterRoleOperation::Set {
                        config_name,
                        config_value: SetConfigValue::Default,
                        in_database,
                    }
                } else if let Ok(expr) = self.parse_expr() {
                    AlterRoleOperation::Set {
                        config_name,
                        config_value: SetConfigValue::Value(expr),
                        in_database,
                    }
                } else {
                    self.expected_ref("config value", self.peek_token_ref())?
                }
            } else {
                self.expected_ref("'TO' or '=' or 'FROM CURRENT'", self.peek_token_ref())?
            }
        } else if self.parse_keyword(Keyword::RESET) {
            if self.parse_keyword(Keyword::ALL) {
                AlterRoleOperation::Reset {
                    config_name: ResetConfig::ALL,
                    in_database,
                }
            } else {
                let config_name = self.parse_object_name(false)?;
                AlterRoleOperation::Reset {
                    config_name: ResetConfig::ConfigName(config_name),
                    in_database,
                }
            }
        } else {
            // [ WITH ]
            let _ = self.parse_keyword(Keyword::WITH);
            let mut options = vec![];
            while let Some(opt) = self.maybe_parse(|parser| parser.parse_pg_role_option())? {
                options.push(opt);
            }
            if options.is_empty() {
                return self.expected_ref("option", self.peek_token_ref())?;
            }

            AlterRoleOperation::WithOptions { options }
        };

        Ok(Statement::AlterRole {
            name: role_name,
            operation,
        })
    }

    fn parse_pg_role_option(&mut self) -> Result<RoleOption, ParserError> {
        let option = match self.parse_one_of_keywords(&[
            Keyword::BYPASSRLS,
            Keyword::NOBYPASSRLS,
            Keyword::CONNECTION,
            Keyword::CREATEDB,
            Keyword::NOCREATEDB,
            Keyword::CREATEROLE,
            Keyword::NOCREATEROLE,
            Keyword::INHERIT,
            Keyword::NOINHERIT,
            Keyword::LOGIN,
            Keyword::NOLOGIN,
            Keyword::PASSWORD,
            Keyword::REPLICATION,
            Keyword::NOREPLICATION,
            Keyword::SUPERUSER,
            Keyword::NOSUPERUSER,
            Keyword::VALID,
        ]) {
            Some(Keyword::BYPASSRLS) => RoleOption::BypassRLS(true),
            Some(Keyword::NOBYPASSRLS) => RoleOption::BypassRLS(false),
            Some(Keyword::CONNECTION) => {
                self.expect_keyword_is(Keyword::LIMIT)?;
                RoleOption::ConnectionLimit(Expr::Value(self.parse_number_value()?))
            }
            Some(Keyword::CREATEDB) => RoleOption::CreateDB(true),
            Some(Keyword::NOCREATEDB) => RoleOption::CreateDB(false),
            Some(Keyword::CREATEROLE) => RoleOption::CreateRole(true),
            Some(Keyword::NOCREATEROLE) => RoleOption::CreateRole(false),
            Some(Keyword::INHERIT) => RoleOption::Inherit(true),
            Some(Keyword::NOINHERIT) => RoleOption::Inherit(false),
            Some(Keyword::LOGIN) => RoleOption::Login(true),
            Some(Keyword::NOLOGIN) => RoleOption::Login(false),
            Some(Keyword::PASSWORD) => {
                let password = if self.parse_keyword(Keyword::NULL) {
                    Password::NullPassword
                } else {
                    Password::Password(Expr::Value(self.parse_value()?))
                };
                RoleOption::Password(password)
            }
            Some(Keyword::REPLICATION) => RoleOption::Replication(true),
            Some(Keyword::NOREPLICATION) => RoleOption::Replication(false),
            Some(Keyword::SUPERUSER) => RoleOption::SuperUser(true),
            Some(Keyword::NOSUPERUSER) => RoleOption::SuperUser(false),
            Some(Keyword::VALID) => {
                self.expect_keyword_is(Keyword::UNTIL)?;
                RoleOption::ValidUntil(Expr::Value(self.parse_value()?))
            }
            _ => self.expected_ref("option", self.peek_token_ref())?,
        };

        Ok(option)
    }
}
