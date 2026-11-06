/**
 * 设置页「安全 → 出站脱敏」面板（`GET/PUT /api/masking`）。
 *
 * 与「指纹脱敏」不是一回事：那边是**不可逆**的指纹改写（只改发送副本，回复不还原），
 * 这里是**可逆**脱敏 —— 出站前把密钥 / 个人信息 / 自定义词换成占位符，回复里的
 * 占位符再还原成原文。两者的开关与失败语义都不同（这里 fail-closed：脱敏器不可用
 * 时拒绝发送），所以界面上分成两块、各自保存。
 *
 * 三项互相独立：只开密钥就只盖密钥，邮箱与自定义词原样上行。词表非空本身也算开启。
 *
 * 草稿（未保存的编辑）留在本组件里，快照里只有「生效值」—— 与指纹脱敏面板同一约定。
 * 词表在保存时 trim / 去空 / 去重（与后端 `parse_words` 同口径），所以编辑期间允许
 * 出现空行，落库的永远是干净的词表。
 */

import * as React from 'react'
import { Alert, Badge, Button, Card, CardContent, CardHeader, Input, Label, Switch } from '@ui'
import { t } from '../i18n'
import { refreshMasking, saveMasking, type SettingsSnapshot } from './settings-state'

/** trim / 去空 / 去重：保存与「是否有改动」都用这一份归一化结果 */
function normalizeWords(list: string[]): string[] {
  const words: string[] = []
  for (const item of list) {
    const word = item.trim()
    if (!word || words.includes(word)) continue
    words.push(word)
  }
  return words
}

export function MaskingPanel({ snap }: { snap: SettingsSnapshot }) {
  const state = snap.masking
  const busy = snap.busy === 'masking'
  const ready = state.status === 'ready'
  const disabled = !ready || busy

  const [secrets, setSecrets] = React.useState(state.secrets)
  const [pii, setPii] = React.useState(state.pii)
  const [words, setWords] = React.useState<string[]>(state.words)

  React.useEffect(() => {
    setSecrets(state.secrets)
    setPii(state.pii)
    setWords(state.words)
  }, [state])

  const cleanWords = normalizeWords(words)
  const dirty = ready && (secrets !== state.secrets
    || pii !== state.pii
    || cleanWords.join('\n') !== state.words.join('\n'))

  function addWord(): void {
    setWords(current => [...current, ''])
  }

  function updateWord(index: number, value: string): void {
    setWords(current => current.map((word, at) => (at === index ? value : word)))
  }

  function removeWord(index: number): void {
    setWords(current => current.filter((_, at) => at !== index))
  }

  async function save(): Promise<void> {
    await saveMasking({ maskSecrets: secrets, maskPii: pii, maskWords: cleanWords })
  }

  const active = state.secrets || state.pii || state.words.length > 0
  const statusText = !ready
    ? (state.status === 'loading' ? '—' : t('未能读取出站脱敏设置，请稍后重试'))
    : active
      ? t('出站前会替换命中的密钥与个人信息，并在回复里还原；自定义词 {count} 个。', { count: state.words.length })
      : t('未开启：出站内容按原样发送（不做可逆脱敏）。')

  return (
    <Card render={<section />} className='panel'>
      <CardHeader className='panel-head'>
        <h2>{t('出站脱敏')}</h2>
        <Badge variant={!ready ? (state.status === 'loading' ? 'outline' : 'destructive') : active ? 'success' : 'outline'}>
          {!ready ? (state.status === 'loading' ? t('检测中…') : t('不可用')) : active ? t('已开启') : t('已关闭')}
        </Badge>
        <div className='head-actions'>
          <Button id='btn-masking-refresh' variant='outline' size='sm' disabled={busy}
            onClick={() => void refreshMasking()}>{t('刷新')}</Button>
        </div>
      </CardHeader>
      <CardContent className='panel-body flex flex-col gap-3'>
        <div className='retention-list'>
          <div className='retention-row'>
            <Label htmlFor='settings-mask-secrets'>{t('密钥脱敏')}</Label>
            <Switch id='settings-mask-secrets' checked={secrets} disabled={disabled}
              onCheckedChange={setSecrets} />
          </div>
          <div className='hint'>{t('开启后，sk-… / ghp_… 这类密钥在发给上游前换成占位符。')}</div>
          <div className='retention-row'>
            <Label htmlFor='settings-mask-pii'>{t('个人信息脱敏')}</Label>
            <Switch id='settings-mask-pii' checked={pii} disabled={disabled}
              onCheckedChange={setPii} />
          </div>
          <div className='hint'>{t('开启后，邮箱等个人信息在发给上游前换成占位符。')}</div>
        </div>

        <div className='retention-list'>
          <div className='flex flex-wrap items-center gap-2'>
            <Label>{t('自定义脱敏词')}</Label>
            <Badge variant='outline'>{t('{count} 个词', { count: cleanWords.length })}</Badge>
            <Button id='btn-masking-word-add' variant='outline' size='sm' className='ml-auto'
              disabled={disabled} onClick={addWord}>{t('添加自定义词')}</Button>
          </div>
          {words.length ? words.map((word, index) => (
            <div className='flex items-center gap-2' key={index}>
              <Input id={`settings-mask-word-${index}`} value={word} disabled={disabled}
                aria-label={t('自定义脱敏词 {n}', { n: index + 1 })}
                placeholder={t('例如：项目代号')}
                onChange={event => updateWord(index, event.target.value)} />
              <Button id={`btn-masking-word-remove-${index}`} variant='ghost' size='sm'
                disabled={disabled} aria-label={t('删除这一项')}
                onClick={() => removeWord(index)}>×</Button>
            </div>
          )) : <div className='hint'>{t('还没有自定义脱敏词。')}</div>}
          <div className='hint'>{t('每行一个词，按字面匹配；保存时自动去掉首尾空白、空行与重复项。')}</div>
        </div>

        <div className='settings-state'>{statusText}</div>
        {!state.saved ? <Alert variant='destructive' role='alert' className='text-xs text-destructive'>
          {t('出站脱敏设置仅本次运行生效，写入数据库失败，请检查配置目录')}
        </Alert> : null}
        {ready ? <div className='flex flex-wrap items-center justify-end gap-2'>
          <span className='hint mr-auto'>{dirty ? t('有未保存的修改') : t('当前设置已保存')}</span>
          <Button id='btn-masking-save' disabled={busy || !dirty} onClick={() => void save()}>
            {busy ? t('保存中…') : t('保存设置')}
          </Button>
        </div> : null}
        <div className='hint retention-note'>{t('出站前替换、回复里还原；脱敏器不可用时请求会被拒绝发送（fail-closed），不会明文外发。')}</div>
      </CardContent>
    </Card>
  )
}
