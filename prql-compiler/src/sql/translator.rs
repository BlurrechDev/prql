//! This module is responsible for translating PRQL AST to sqlparser AST, and
//! then to a String. We use sqlparser because it's trivial to create the string
//! once it's in their AST (it's just `.to_string()`). It also lets us support a
//! few dialects of SQL immediately.
// The average code quality here is low — we're basically plugging in test
// cases and fixing what breaks, with some occasional refactors. I'm not sure
// that's a terrible approach — the SQL spec is huge, so we're not reasonably
// going to be isomorphically mapping everything back from SQL to PRQL. But it
// does mean we should continue to iterate on this file and refactor things when
// necessary.
use anyhow::{anyhow, Result};
use itertools::Itertools;
use sqlformat::{format, FormatOptions, QueryParams};
use sqlparser::ast::{self as sql_ast, Select, SetExpr, TableWithJoins};

use crate::ast::pl::{DialectHandler, Literal};
use crate::ast::rq::{CId, Expr, ExprKind, IrFold, Query, Relation, TableDecl, Transform};
use crate::sql::anchor::materialize_inputs;
use crate::utils::{IntoOnly, Pluck, TableCounter};

use super::codegen::*;
use super::context::AnchorContext;
use super::preprocess::{preprocess_distinct, preprocess_reorder};
use super::{anchor, sample_data};

pub(super) struct Context {
    pub dialect: Box<dyn DialectHandler>,
    pub anchor: AnchorContext,

    pub omit_ident_prefix: bool,

    /// True iff codegen should generate expressions before SELECT's projection is applied.
    /// For example:
    /// - WHERE needs `pre_projection=true`, but
    /// - ORDER BY needs `pre_projection=false`.
    pub pre_projection: bool,
}

/// Translate a PRQL AST into a SQL string.
pub fn translate(query: Query) -> Result<String> {
    let sql_query = translate_query(query)?;

    let sql_query_string = sql_query.to_string();

    let formatted = format(
        &sql_query_string,
        &QueryParams::default(),
        FormatOptions::default(),
    );

    // The sql formatter turns `{{` into `{ {`, and while that's reasonable SQL,
    // we want to allow jinja expressions through. So we (somewhat hackily) replace
    // any `{ {` with `{{`.
    let formatted = formatted.replace("{ {", "{{").replace("} }", "}}");

    Ok(formatted)
}

pub fn translate_query(query: Query) -> Result<sql_ast::Query> {
    let dialect = query.def.dialect.handler();

    let (anchor, query) = AnchorContext::of(query);

    let mut context = Context {
        dialect,
        anchor,
        omit_ident_prefix: false,
        pre_projection: false,
    };

    // extract tables and the pipeline
    let tables = into_tables(query.relation, query.tables, &mut context)?;

    // preprocess & split into atomics
    let mut atomics = Vec::new();
    for table in tables {
        let name = table
            .name
            .unwrap_or_else(|| context.anchor.gen_table_name());

        match table.relation {
            Relation::Pipeline(pipeline) => {
                // preprocess
                let pipeline = preprocess_distinct(pipeline, &mut context)?;
                let pipeline = preprocess_reorder(pipeline);

                // split to atomics
                atomics.extend(split_into_atomics(name, pipeline, &mut context.anchor));
            }
            Relation::Literal(_, _) | Relation::SString(_, _) => atomics.push(AtomicQuery {
                name,
                relation: table.relation,
            }),
            Relation::ExternRef(_, _) => {
                // ref does not need it's own CTE
            }
        }
    }

    // take last table
    let main_query = atomics.remove(atomics.len() - 1);
    let ctes = atomics;

    // convert each of the CTEs
    let ctes: Vec<_> = ctes
        .into_iter()
        .map(|t| table_to_sql_cte(t, &mut context))
        .try_collect()?;

    // convert main query
    let mut main_query = sql_query_of_relation(main_query.relation, &mut context)?;

    // attach CTEs
    if !ctes.is_empty() {
        main_query.with = Some(sql_ast::With {
            cte_tables: ctes,
            recursive: false,
        });
    }

    Ok(main_query)
}

/// A query that can be expressed with one SELECT statement
#[derive(Debug)]
pub struct AtomicQuery {
    name: String,
    relation: Relation,
}

fn into_tables(
    main_pipeline: Relation,
    tables: Vec<TableDecl>,
    context: &mut Context,
) -> Result<Vec<TableDecl>> {
    let main = TableDecl {
        id: context.anchor.tid.gen(),
        name: None,
        relation: main_pipeline,
    };
    Ok([tables, vec![main]].concat())
}

fn table_to_sql_cte(table: AtomicQuery, context: &mut Context) -> Result<sql_ast::Cte> {
    let alias = sql_ast::TableAlias {
        name: translate_ident_part(table.name, context),
        columns: vec![],
    };
    Ok(sql_ast::Cte {
        alias,
        query: Box::new(sql_query_of_relation(table.relation, context)?),
        from: None,
    })
}

fn sql_query_of_relation(relation: Relation, context: &mut Context) -> Result<sql_ast::Query> {
    match relation {
        Relation::ExternRef(_, _) => unreachable!(),
        Relation::Pipeline(pipeline) => sql_query_of_pipeline(pipeline, context),
        Relation::Literal(lit, _) => Ok(sample_data::sql_of_sample_data(&lit)),
        Relation::SString(items, _) => translate_query_sstring(items, context),
    }
}

fn sql_query_of_pipeline(
    pipeline: Vec<Transform>,
    context: &mut Context,
) -> Result<sql_ast::Query> {
    let mut counter = TableCounter::default();
    let mut pipeline = counter.fold_transforms(pipeline)?;
    context.omit_ident_prefix = counter.count() == 1;
    log::debug!("atomic query contains {} tables", counter.count());

    context.pre_projection = true;

    let projection = pipeline
        .pluck(|t| t.into_select())
        .into_only()
        .unwrap_or_default()
        .into_iter()
        .map(|id| translate_select_item(id, context))
        .try_collect()?;

    let mut from = pipeline
        .pluck(|t| t.into_from())
        .into_iter()
        .map(|source| TableWithJoins {
            relation: table_factor_of_tid(source, context),
            joins: vec![],
        })
        .collect::<Vec<_>>();

    let joins = pipeline
        .pluck(|t| t.into_join())
        .into_iter()
        .map(|j| translate_join(j, context))
        .collect::<Result<Vec<_>>>()?;
    if !joins.is_empty() {
        if let Some(from) = from.last_mut() {
            from.joins = joins;
        } else {
            return Err(anyhow!("Cannot use `join` without `from`"));
        }
    }

    // Split the pipeline into before & after the aggregate
    let aggregate_position = pipeline
        .iter()
        .position(|t| matches!(t, Transform::Aggregate { .. }))
        .unwrap_or(pipeline.len());
    let (before, after) = pipeline.split_at(aggregate_position);

    // WHERE and HAVING
    let where_ = filter_of_pipeline(before, context)?;
    let having = filter_of_pipeline(after, context)?;

    // GROUP BY
    let aggregate = pipeline.get(aggregate_position);
    let group_by: Vec<CId> = aggregate
        .map(|t| match t {
            Transform::Aggregate { partition, .. } => partition.clone(),
            _ => unreachable!(),
        })
        .unwrap_or_default();
    let group_by = try_into_exprs(group_by, context)?;

    context.pre_projection = false;

    let takes = pipeline.pluck(|t| t.into_take());
    let ranges = takes.into_iter().map(|x| x.range).collect();
    let take = range_of_ranges(ranges)?;
    let offset = take.start.map(|s| s - 1).unwrap_or(0);
    let limit = take.end.map(|e| e - offset);

    let offset = if offset == 0 {
        None
    } else {
        Some(sqlparser::ast::Offset {
            value: translate_expr_kind(ExprKind::Literal(Literal::Integer(offset)), context)?,
            rows: sqlparser::ast::OffsetRows::None,
        })
    };

    // Use sorting from the frame
    let order_by = pipeline
        .pluck(|t| t.into_sort())
        .last()
        .map(|sorts| {
            sorts
                .iter()
                .map(|s| translate_column_sort(s, context))
                .try_collect()
        })
        .transpose()?
        .unwrap_or_default();

    let distinct = pipeline.iter().any(|t| matches!(t, Transform::Unique));

    Ok(sql_ast::Query {
        body: Box::new(SetExpr::Select(Box::new(Select {
            distinct,
            top: if context.dialect.use_top() {
                limit.map(|l| top_of_i64(l, context))
            } else {
                None
            },
            projection,
            into: None,
            from,
            lateral_views: vec![],
            selection: where_,
            group_by,
            cluster_by: vec![],
            distribute_by: vec![],
            sort_by: vec![],
            having,
            qualify: None,
        }))),
        order_by,
        with: None,
        limit: if context.dialect.use_top() {
            None
        } else {
            limit.map(expr_of_i64)
        },
        offset,
        fetch: None,
        lock: None,
    })
}

fn split_into_atomics(
    name: String,
    mut pipeline: Vec<Transform>,
    context: &mut AnchorContext,
) -> Vec<AtomicQuery> {
    materialize_inputs(&pipeline, context);

    let output_cols = context.determine_select_columns(&pipeline);
    let mut required_cols = output_cols.clone();

    // split pipeline, back to front
    let mut parts_rev = Vec::new();
    loop {
        let (preceding, split) = anchor::split_off_back(context, required_cols, pipeline);

        if let Some((preceding, cols_at_split)) = preceding {
            log::debug!(
                "pipeline split after {}",
                preceding.last().unwrap().as_ref()
            );
            parts_rev.push((split, cols_at_split.clone()));

            pipeline = preceding;
            required_cols = cols_at_split;
        } else {
            parts_rev.push((split, Vec::new()));
            break;
        }
    }
    parts_rev.reverse();
    let mut parts = parts_rev;

    // sometimes, additional columns will be added into select, which have to
    // be filtered out here, using additional CTE
    if let Some((pipeline, _)) = parts.last() {
        let select_cols = pipeline.first().unwrap().as_select().unwrap();

        if select_cols != &output_cols {
            parts.push((vec![Transform::Select(output_cols)], select_cols.clone()));
        }
    }

    // add names to pipelines, anchor, front to back
    let mut atomics = Vec::with_capacity(parts.len());
    let last = parts.pop().unwrap();

    let last_pipeline = if parts.is_empty() {
        last.0
    } else {
        // this code chunk is bloated but I cannot find a more concise alternative
        let first = parts.remove(0);

        let first_name = context.gen_table_name();
        atomics.push(AtomicQuery {
            name: first_name.clone(),
            relation: Relation::Pipeline(first.0),
        });

        let mut prev_name = first_name;
        for (pipeline, cols_before) in parts.into_iter() {
            let name = context.gen_table_name();
            let pipeline = anchor::anchor_split(context, &prev_name, &cols_before, pipeline);

            atomics.push(AtomicQuery {
                name: name.clone(),
                relation: Relation::Pipeline(pipeline),
            });

            prev_name = name;
        }

        anchor::anchor_split(context, &prev_name, &last.1, last.0)
    };
    atomics.push(AtomicQuery {
        name,
        relation: Relation::Pipeline(last_pipeline),
    });

    atomics
}

fn filter_of_pipeline(
    pipeline: &[Transform],
    context: &mut Context,
) -> Result<Option<sql_ast::Expr>> {
    let filters: Vec<Expr> = pipeline
        .iter()
        .filter_map(|t| match &t {
            Transform::Filter(filter) => Some(filter.clone()),
            _ => None,
        })
        .collect();
    filter_of_filters(filters, context)
}

#[cfg(test)]
mod test {
    use insta::assert_snapshot;

    use super::*;
    use crate::{ast::pl::GenericDialect, parse, semantic::resolve};

    fn parse_and_resolve(prql: &str) -> Result<(Vec<Transform>, Context)> {
        let query = resolve(parse(prql)?)?;
        let (anchor, query) = AnchorContext::of(query);
        let context = Context {
            dialect: Box::new(GenericDialect {}),
            anchor,
            omit_ident_prefix: false,
            pre_projection: false,
        };

        let pipeline = query.relation.into_pipeline().unwrap();

        Ok((preprocess_reorder(pipeline), context))
    }

    #[test]
    fn test_ctes_of_pipeline() {
        // One aggregate, take at the end
        let prql: &str = r###"
        from employees
        filter country == "USA"
        aggregate [sal = average salary]
        sort sal
        take 20
        "###;

        let (pipeline, mut context) = parse_and_resolve(prql).unwrap();
        let queries = split_into_atomics("".to_string(), pipeline, &mut context.anchor);
        assert_eq!(queries.len(), 1);

        // One aggregate, but take at the top
        let prql: &str = r###"
        from employees
        take 20
        filter country == "USA"
        aggregate [sal = average salary]
        sort sal
        "###;

        let (pipeline, mut context) = parse_and_resolve(prql).unwrap();
        let queries = split_into_atomics("".to_string(), pipeline, &mut context.anchor);
        assert_eq!(queries.len(), 2);

        // A take, then two aggregates
        let prql: &str = r###"
        from employees
        take 20
        filter country == "USA"
        aggregate [sal = average salary]
        aggregate [sal2 = average sal]
        sort sal2
        "###;

        let (pipeline, mut context) = parse_and_resolve(prql).unwrap();
        let queries = split_into_atomics("".to_string(), pipeline, &mut context.anchor);
        assert_eq!(queries.len(), 3);

        // A take, then a select
        let prql: &str = r###"
        from employees
        take 20
        select first_name
        "###;

        let (pipeline, mut context) = parse_and_resolve(prql).unwrap();
        let queries = split_into_atomics("".to_string(), pipeline, &mut context.anchor);
        assert_eq!(queries.len(), 1);
    }

    #[test]
    fn test_variable_after_aggregate() {
        let query = &r#"
        from employees
        group [title, emp_no] (
            aggregate [emp_salary = average salary]
        )
        group [title] (
            aggregate [avg_salary = average emp_salary]
        )
        "#;

        let query = resolve(parse(query).unwrap()).unwrap();

        let sql_ast = translate(query).unwrap();

        assert_snapshot!(sql_ast);
    }

    #[test]
    fn test_derive_filter() {
        // I suspect that the anchoring algorithm has a architectural flaw:
        // it assumes that it can materialize all columns, even if their
        // Compute is in a prior CTE. The problem is that because anchoring is
        // computed back-to-front, we don't know where Compute will end up when
        // materializing following transforms.
        //
        // If algorithm is changed to be front-to-back, preprocess_reorder can
        // be (must be) removed.

        let query = &r#"
        from employees
        derive global_rank = rank
        filter country == "USA"
        derive rank = rank
        "#;

        let query = resolve(parse(query).unwrap()).unwrap();

        let sql_ast = translate(query).unwrap();

        assert_snapshot!(sql_ast, @r###"
        WITH table_1 AS (
          SELECT
            *,
            RANK() OVER () AS global_rank
          FROM
            employees
        )
        SELECT
          *,
          global_rank,
          RANK() OVER () AS rank
        FROM
          table_1
        WHERE
          country = 'USA'
        "###);
    }

    #[test]
    fn test_filter_windowed() {
        // #806
        let query = &r#"
        from tbl1
        filter (average bar) > 3
        "#;

        assert_snapshot!(crate::compile(query).unwrap(), @r###"
        WITH table_1 AS (
          SELECT
            *,
            AVG(bar) OVER () AS _expr_0
          FROM
            tbl1
        )
        SELECT
          *
        FROM
          table_1
        WHERE
          _expr_0 > 3
        "###);
    }

    #[test]
    fn test_relation_literal() {
        let rq = &r#"
        {
            "def": {
              "version": null,
              "dialect": "Generic"
            },
            "tables": [
              {
                "id": 0,
                "name": "sample_table",
                "relation": {
                  "Literal": [
                    {
                      "columns": ["a", "b", "c"],
                      "rows": [
                        ["3", "a", "false"],
                        ["5", "c", "true"],
                        ["1", "b", "true"],
                        ["2", "a", "false"]
                      ]
                    },
                    [
                      {
                        "id": 0,
                        "kind": "Wildcard"
                      },
                      {
                        "id": 1,
                        "kind": {
                          "ExternRef": "a"
                        }
                      },
                      {
                        "id": 2,
                        "kind": {
                          "ExternRef": "b"
                        }
                      },
                      {
                        "id": 3,
                        "kind": {
                          "ExternRef": "c"
                        }
                      }
                    ]
                  ]
                }
              }
            ],
            "relation": {
              "Pipeline": [
                {
                  "From": {
                    "source": 0,
                    "columns": [
                      {
                        "id": 4,
                        "kind": "Wildcard"
                      },
                      {
                        "id": 5,
                        "kind": {
                          "Expr": {
                            "name": "a",
                            "expr": {
                              "kind": {
                                "ColumnRef": 1
                              },
                              "span": null
                            }
                          }
                        }
                      },
                      {
                        "id": 6,
                        "kind": {
                          "Expr": {
                            "name": "b",
                            "expr": {
                              "kind": {
                                "ColumnRef": 2
                              },
                              "span": null
                            }
                          }
                        }
                      },
                      {
                        "id": 7,
                        "kind": {
                          "Expr": {
                            "name": "c",
                            "expr": {
                              "kind": {
                                "ColumnRef": 3
                              },
                              "span": null
                            }
                          }
                        }
                      }
                    ],
                    "name": null
                  }
                },
                {
                  "Compute": {
                    "id": 8,
                    "kind": {
                      "Expr": {
                        "name": "full_name",
                        "expr": {
                          "kind": {
                            "FString": [
                              {
                                "Expr": {
                                  "kind": {
                                    "ColumnRef": 5
                                  },
                                  "span": {
                                    "start": 37,
                                    "end": 41
                                  }
                                }
                              },
                              {
                                "String": " "
                              },
                              {
                                "Expr": {
                                  "kind": {
                                    "ColumnRef": 6
                                  },
                                  "span": {
                                    "start": 44,
                                    "end": 51
                                  }
                                }
                              }
                            ]
                          },
                          "span": {
                            "start": 22,
                            "end": 53
                          }
                        }
                      }
                    }
                  }
                },
                {
                  "Filter": {
                    "kind": {
                      "Binary": {
                        "left": {
                          "kind": {
                            "ColumnRef": 7
                          },
                          "span": {
                            "start": 61,
                            "end": 65
                          }
                        },
                        "op": "Gt",
                        "right": {
                          "kind": {
                            "Literal": {
                              "Integer": 2
                            }
                          },
                          "span": {
                            "start": 67,
                            "end": 68
                          }
                        }
                      }
                    },
                    "span": {
                      "start": 61,
                      "end": 68
                    }
                  }
                },
                {
                  "Select": [
                    4,
                    8
                  ]
                }
              ]
            }
          }
        "#;

        assert_snapshot!(crate::json_to_rq(rq).and_then(translate).unwrap(), @r###"
        WITH sample_table AS (
          SELECT
            3 AS a,
            a AS b,
            false AS c
          UNION
          ALL
          SELECT
            5 AS a,
            c AS b,
            true AS c
          UNION
          ALL
          SELECT
            1 AS a,
            b AS b,
            true AS c
          UNION
          ALL
          SELECT
            2 AS a,
            a AS b,
            false AS c
        ),
        table_2 AS (
          SELECT
            *,
            CONCAT(a, ' ', b) AS full_name,
            c
          FROM
            sample_table AS table_0
        )
        SELECT
          *,
          full_name
        FROM
          table_2
        WHERE
          c > 2
        "###);
    }
}
